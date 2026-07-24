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
pub fn extract_image(
    src: &Path,
    seek_ms: Option<u64>,
    width: u32,
    quality: i32,
    out: &Path,
) -> Result<(), FrameError> {
    let spec = format!("scale={width}:-2");
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

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg_the_third as ffmpeg;

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
}
