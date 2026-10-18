//! In-process single-frame image extraction — replaces the `ffmpeg -ss
//! … -frames:v 1 -vf scale=W:-2 -q:v N -f mjpeg` spawn in
//! `pharos-cache::image_cache`. Seek, decode one frame, scale, MJPEG.

use super::frames::{self, FrameError};
use ffmpeg::format::Pixel;
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Mean luma (Y-plane average, 0–255) below which a frame is treated as
/// blank/black and skipped when choosing a poster. A fade-to-black or an
/// opening title card sits near 0; a real scene is comfortably above 20 even
/// when dimly lit, so this only rejects genuinely empty frames.
const BLACK_LUMA_THRESHOLD: u32 = 20;

/// Cap on how many frames past the seek point we decode looking for a
/// non-black one. At ~24 fps this is ~12 s of runtime — long enough to clear
/// a title sequence, bounded so a wholly-dark passage can't decode forever.
/// If none clears the threshold we fall back to the brightest frame seen.
const MAX_SCAN_FRAMES: usize = 300;

/// Average of the Y (luma) plane of a YUV frame, 0–255. Every 4th pixel is
/// sampled — a poster-selection heuristic doesn't need per-pixel precision and
/// the stride walk over a full-res plane is the hot path. Returns 0 for an
/// empty plane.
pub(crate) fn mean_luma(f: &ffmpeg::frame::Video) -> u32 {
    let data = f.data(0);
    let stride = f.stride(0);
    let w = f.width() as usize;
    let h = f.height() as usize;
    if w == 0 || h == 0 || stride == 0 {
        return 0;
    }
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    let mut y = 0;
    while y < h {
        let row = &data[y * stride..y * stride + w];
        let mut x = 0;
        while x < w {
            sum += row[x] as u64;
            count += 1;
            x += 4;
        }
        y += 1;
    }
    // `checked_div` yields None for an all-empty plane (count 0); the mean of
    // u8 samples is ≤ 255 so the cast never truncates.
    sum.checked_div(count).unwrap_or(0) as u32
}

/// Extract a single JPEG frame from `src` at `seek_ms` (input seek; `None`
/// = start), scaled to `width` px wide (aspect-preserved, even height),
/// written to `out`. `quality` is the FFmpeg `-q:v` value (lower = better;
/// 3 matches the spawn path).
///
/// The frame at the exact seek point is often blank — many films open on a
/// black title card or fade, so a fixed-timestamp grab yields an all-black
/// poster/thumbnail. Rather than grab the first frame unconditionally, decode
/// forward and pick the first frame whose mean luma clears
/// [`BLACK_LUMA_THRESHOLD`], scanning at most [`MAX_SCAN_FRAMES`]. If none
/// clears it (a genuinely dark passage), the brightest frame seen is used, so
/// the result is never worse than the naive first-frame grab.
/// Filter prefix that crops a packed 3D frame to the left/top eye and resets
/// SAR before scaling. For Half-SBS/Half-TAB, we also restore display aspect
/// ratio (`scale=iw*2:ih` or `scale=iw:ih*2`) before the target-width scale
/// `scale={w}:-2` so the thumbnail is not squeezed 2:1!
pub fn stereo_left_eye_filter_prefix(
    video_3d_format: Option<pharos_core::Video3DFormat>,
) -> Option<&'static str> {
    crate::backend::stereo_left_eye_filter_prefix(video_3d_format)
}

/// Extract a single JPEG frame from `src` at `seek_ms` (input seek; `None`
/// = start), scaled to `width` px wide (aspect-preserved, even height),
/// with optional packed 3D left-eye crop, written to `out`.
pub fn extract_image_3d(
    src: &Path,
    seek_ms: Option<u64>,
    width: u32,
    quality: i32,
    video_3d_format: Option<pharos_core::Video3DFormat>,
    out: &Path,
) -> Result<(), FrameError> {
    let spec = match stereo_left_eye_filter_prefix(video_3d_format) {
        Some(prefix) => format!("{prefix},scale={width}:-2"),
        None => format!("scale={width}:-2"),
    };
    let mut chosen: Option<Vec<u8>> = None;
    let mut brightest: Option<(u32, ffmpeg::frame::Video)> = None;
    let mut scanned = 0usize;
    // Not keyframe-only: a poster/thumbnail wants a frame near the seek point,
    // so decode forward rather than snapping to a keyframe.
    frames::filter_video(src, seek_ms, &spec, Pixel::YUVJ420P, false, |f| {
        let luma = mean_luma(f);
        if luma >= BLACK_LUMA_THRESHOLD {
            // First non-black frame — encode it and stop.
            chosen = Some(frames::encode_jpeg(f, quality)?);
            return Ok(false);
        }
        // Too dark: remember it only if it's the brightest so far, as a
        // fallback for a passage that never clears the threshold.
        let brighter = match &brightest {
            Some((b, _)) => luma > *b,
            None => true,
        };
        if brighter {
            brightest = Some((luma, f.clone()));
        }
        scanned += 1;
        // Stop once the scan budget is spent; the brightest frame is used.
        Ok(scanned < MAX_SCAN_FRAMES)
    })?;
    let bytes = match chosen {
        Some(b) => b,
        None => {
            // No frame cleared the threshold — encode the brightest we saw.
            let (_, frame) =
                brightest.ok_or_else(|| FrameError::BadInput("no frame decoded".into()))?;
            frames::encode_jpeg(&frame, quality)?
        }
    };
    std::fs::write(out, &bytes)
        .map_err(|e| FrameError::Other(format!("write {}: {e}", out.display())))?;
    Ok(())
}

/// Extract a single JPEG frame from `src` at `seek_ms` (input seek; `None`
/// = start), scaled to `width` px wide (aspect-preserved, even height),
/// written to `out`. `quality` is the FFmpeg `-q:v` value (lower = better;
/// 3 matches the spawn path).
pub fn extract_image(
    src: &Path,
    seek_ms: Option<u64>,
    width: u32,
    quality: i32,
    out: &Path,
) -> Result<(), FrameError> {
    extract_image_3d(src, seek_ms, width, quality, None, out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ffmpeg_the_third as ffmpeg;
    use pharos_core::Video3DFormat;

    /// Build a YUVJ420p frame whose Y plane is a uniform value.
    fn solid_frame(w: u32, h: u32, luma: u8) -> ffmpeg::frame::Video {
        let mut f = ffmpeg::frame::Video::new(Pixel::YUVJ420P, w, h);
        let stride = f.stride(0);
        let width = f.width() as usize;
        let height = f.height() as usize;
        let data = f.data_mut(0);
        for y in 0..height {
            for x in 0..width {
                data[y * stride + x] = luma;
            }
        }
        f
    }

    /// Build an SBS frame: left half has `left_luma`, right half has `right_luma`.
    fn sbs_frame(w: u32, h: u32, left_luma: u8, right_luma: u8) -> ffmpeg::frame::Video {
        let mut f = ffmpeg::frame::Video::new(Pixel::YUVJ420P, w, h);
        let stride = f.stride(0);
        let width = f.width() as usize;
        let height = f.height() as usize;
        let data = f.data_mut(0);
        for y in 0..height {
            for x in 0..width {
                data[y * stride + x] = if x < width / 2 { left_luma } else { right_luma };
            }
        }
        if f.planes() > 1 {
            let u_stride = f.stride(1);
            let u_data = f.data_mut(1);
            for y in 0..(height / 2) {
                for x in 0..(width / 2) {
                    u_data[y * u_stride + x] = 128;
                }
            }
        }
        if f.planes() > 2 {
            let v_stride = f.stride(2);
            let v_data = f.data_mut(2);
            for y in 0..(height / 2) {
                for x in 0..(width / 2) {
                    v_data[y * v_stride + x] = 128;
                }
            }
        }
        f
    }

    /// Build a TAB frame: top half has `top_luma`, bottom half has `bottom_luma`.
    fn tab_frame(w: u32, h: u32, top_luma: u8, bottom_luma: u8) -> ffmpeg::frame::Video {
        let mut f = ffmpeg::frame::Video::new(Pixel::YUVJ420P, w, h);
        let stride = f.stride(0);
        let width = f.width() as usize;
        let height = f.height() as usize;
        let data = f.data_mut(0);
        for y in 0..height {
            for x in 0..width {
                data[y * stride + x] = if y < height / 2 {
                    top_luma
                } else {
                    bottom_luma
                };
            }
        }
        if f.planes() > 1 {
            let u_stride = f.stride(1);
            let u_data = f.data_mut(1);
            for y in 0..(height / 2) {
                for x in 0..(width / 2) {
                    u_data[y * u_stride + x] = 128;
                }
            }
        }
        if f.planes() > 2 {
            let v_stride = f.stride(2);
            let v_data = f.data_mut(2);
            for y in 0..(height / 2) {
                for x in 0..(width / 2) {
                    v_data[y * v_stride + x] = 128;
                }
            }
        }
        f
    }

    fn run_filter(frame: &ffmpeg::frame::Video, spec: &str) -> ffmpeg::frame::Video {
        crate::libav::init().expect("libav init");
        let mut graph = ffmpeg::filter::Graph::new();
        let args = format!(
            "video_size={}x{}:pix_fmt=yuvj420p:time_base=1/1000:pixel_aspect=1/1",
            frame.width(),
            frame.height(),
        );
        graph
            .add(
                &ffmpeg::filter::find("buffer").expect("buffer filter"),
                "in",
                &args,
            )
            .expect("add in");
        graph
            .add(
                &ffmpeg::filter::find("buffersink").expect("buffersink filter"),
                "out",
                "",
            )
            .expect("add out");
        if let Some(mut out) = graph.get("out") {
            out.set_pixel_format(Pixel::YUVJ420P);
        }
        graph
            .output("in", 0)
            .and_then(|p| p.input("out", 0))
            .and_then(|p| p.parse(spec))
            .expect("parse filter");
        graph.validate().expect("validate graph");

        let frame_clone = frame.clone();
        if let Some(mut src) = graph.get("in") {
            src.source().add(&frame_clone).expect("add frame");
        }
        if let Some(mut src) = graph.get("in") {
            src.source().flush().expect("flush src");
        }

        let mut out_frame = ffmpeg::frame::Video::empty();
        if let Some(mut sink) = graph.get("out") {
            sink.sink().frame(&mut out_frame).expect("sink frame");
        }
        out_frame
    }

    #[test]
    fn mean_luma_of_black_is_zero() {
        assert_eq!(mean_luma(&solid_frame(64, 64, 0)), 0);
    }

    #[test]
    fn mean_luma_of_grey_matches_value() {
        assert_eq!(mean_luma(&solid_frame(64, 64, 128)), 128);
    }

    #[test]
    fn black_frame_is_below_threshold() {
        assert!(mean_luma(&solid_frame(64, 64, 0)) < BLACK_LUMA_THRESHOLD);
        // A dimly-lit scene still clears the blank threshold.
        assert!(mean_luma(&solid_frame(64, 64, 40)) >= BLACK_LUMA_THRESHOLD);
    }

    #[test]
    fn stereo_filter_prefix_specs() {
        assert_eq!(
            stereo_left_eye_filter_prefix(Some(Video3DFormat::HalfSideBySide)),
            Some("crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1")
        );
        assert_eq!(
            stereo_left_eye_filter_prefix(Some(Video3DFormat::FullSideBySide)),
            Some("crop=iw/2:ih:0:0,setsar=1")
        );
        assert_eq!(
            stereo_left_eye_filter_prefix(Some(Video3DFormat::HalfTopAndBottom)),
            Some("crop=iw:ih/2:0:0,scale=iw:ih*2,setsar=1")
        );
        assert_eq!(
            stereo_left_eye_filter_prefix(Some(Video3DFormat::FullTopAndBottom)),
            Some("crop=iw:ih/2:0:0,setsar=1")
        );
        assert_eq!(
            stereo_left_eye_filter_prefix(Some(Video3DFormat::Mvc)),
            None
        );
        assert_eq!(stereo_left_eye_filter_prefix(None), None);
    }

    #[test]
    fn left_eye_crop_sbs_and_tab_synthetic_frames() {
        // SBS frame: left half luma 200, right half luma 20.
        let sbs = sbs_frame(64, 64, 200, 20);
        // Uncropped scale preserves both eyes: mean luma is (200 + 20) / 2 = 110.
        let uncropped = run_filter(&sbs, "scale=32:-2");
        let uncropped_luma = mean_luma(&uncropped);
        assert!(
            (105..=115).contains(&uncropped_luma),
            "uncropped: {uncropped_luma}"
        );

        // Half-SBS: crop left half, restore DAR, scale -> uniform luma ~200.
        let hsbs_prefix =
            stereo_left_eye_filter_prefix(Some(Video3DFormat::HalfSideBySide)).unwrap();
        let hsbs_out = run_filter(&sbs, &format!("{hsbs_prefix},scale=32:-2"));
        let hsbs_luma = mean_luma(&hsbs_out);
        assert!(
            (195..=205).contains(&hsbs_luma),
            "expected ~200, got {hsbs_luma}"
        );

        // Full-SBS: crop left half, scale -> uniform luma ~200.
        let fsbs_prefix =
            stereo_left_eye_filter_prefix(Some(Video3DFormat::FullSideBySide)).unwrap();
        let fsbs_out = run_filter(&sbs, &format!("{fsbs_prefix},scale=32:-2"));
        let fsbs_luma = mean_luma(&fsbs_out);
        assert!(
            (195..=205).contains(&fsbs_luma),
            "expected ~200, got {fsbs_luma}"
        );

        // TAB frame: top half luma 200, bottom half luma 20.
        let tab = tab_frame(64, 64, 200, 20);
        let uncropped_tab = run_filter(&tab, "scale=32:-2");
        let uncropped_tab_luma = mean_luma(&uncropped_tab);
        assert!(
            (105..=115).contains(&uncropped_tab_luma),
            "uncropped tab: {uncropped_tab_luma}"
        );

        // Half-TAB: crop top half, restore DAR, scale -> uniform luma ~200.
        let htab_prefix =
            stereo_left_eye_filter_prefix(Some(Video3DFormat::HalfTopAndBottom)).unwrap();
        let htab_out = run_filter(&tab, &format!("{htab_prefix},scale=32:-2"));
        let htab_luma = mean_luma(&htab_out);
        assert!(
            (195..=205).contains(&htab_luma),
            "expected ~200, got {htab_luma}"
        );

        // Full-TAB: crop top half, scale -> uniform luma ~200.
        let ftab_prefix =
            stereo_left_eye_filter_prefix(Some(Video3DFormat::FullTopAndBottom)).unwrap();
        let ftab_out = run_filter(&tab, &format!("{ftab_prefix},scale=32:-2"));
        let ftab_luma = mean_luma(&ftab_out);
        assert!(
            (195..=205).contains(&ftab_luma),
            "expected ~200, got {ftab_luma}"
        );
    }
}
