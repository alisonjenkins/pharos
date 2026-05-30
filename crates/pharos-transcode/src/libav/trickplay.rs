//! In-process trickplay sprite-sheet generation — replaces the `ffmpeg
//! -vf fps=1/N,scale=W:-2,tile=GxG -frames:v K -f image2` spawn in
//! `pharos-cache::trickplay_cache`. Each filtered output frame is one
//! GxG sprite sheet; we write them 0-based as `{i}.jpg` into `out_dir`.

use super::frames::{self, FrameError};
use ffmpeg::format::Pixel;
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Generate up to `max_sheets` trickplay sprite sheets from `src`.
/// - `interval_ms`: sample one frame every `interval_ms` (the `fps=1/N`).
/// - `width`: per-thumbnail width (aspect-preserved, even height).
/// - `grid`: GxG thumbnails per sheet.
/// - `quality`: FFmpeg `-q:v` (5 matches the spawn path).
///
/// Returns the number of sheets actually written (may be < `max_sheets`
/// on short sources, mirroring the spawn path's tolerance).
#[allow(clippy::too_many_arguments)]
pub fn trickplay_sprite(
    src: &Path,
    interval_ms: u64,
    width: u32,
    grid: u32,
    max_sheets: u32,
    quality: i32,
    out_dir: &Path,
) -> Result<u32, FrameError> {
    if max_sheets == 0 {
        return Err(FrameError::Other("max_sheets = 0".into()));
    }
    std::fs::create_dir_all(out_dir)
        .map_err(|e| FrameError::Other(format!("mkdir {}: {e}", out_dir.display())))?;
    let interval_seconds = interval_ms as f64 / 1000.0;
    let spec = format!(
        "fps=1/{interval_seconds},scale={width}:-2:flags=fast_bilinear,tile={grid}x{grid}:padding=0:margin=0"
    );

    let mut produced: u32 = 0;
    let mut write_err: Option<FrameError> = None;
    frames::filter_video(src, None, &spec, Pixel::YUVJ420P, |f| {
        let bytes = frames::encode_jpeg(f, quality)?;
        let p = out_dir.join(format!("{produced}.jpg"));
        if let Err(e) = std::fs::write(&p, &bytes) {
            write_err = Some(FrameError::Other(format!("write {}: {e}", p.display())));
            return Ok(false);
        }
        produced += 1;
        // Continue until we have enough sheets.
        Ok(produced < max_sheets)
    })?;

    if let Some(e) = write_err {
        return Err(e);
    }
    if produced == 0 {
        return Err(FrameError::BadInput("no sprite sheets produced".into()));
    }
    Ok(produced)
}
