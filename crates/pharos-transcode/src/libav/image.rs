//! In-process single-frame image extraction — replaces the `ffmpeg -ss
//! … -frames:v 1 -vf scale=W:-2 -q:v N -f mjpeg` spawn in
//! `pharos-cache::image_cache`. Seek, decode one frame, scale, MJPEG.

use super::frames::{self, FrameError};
use ffmpeg::format::Pixel;
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

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
    let spec = format!("scale={width}:-2");
    let mut jpeg: Option<Vec<u8>> = None;
    frames::filter_video(src, seek_ms, &spec, Pixel::YUVJ420P, |f| {
        jpeg = Some(frames::encode_jpeg(f, quality)?);
        // One frame is enough for a thumbnail.
        Ok(false)
    })?;
    let bytes = jpeg.ok_or_else(|| FrameError::BadInput("no frame decoded".into()))?;
    std::fs::write(out, &bytes).map_err(|e| FrameError::Other(format!("write {}: {e}", out.display())))?;
    Ok(())
}
