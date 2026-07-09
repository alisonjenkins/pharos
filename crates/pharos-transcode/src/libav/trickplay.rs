//! In-process trickplay sprite-sheet generation — replaces the `ffmpeg
//! -vf fps=1/N,scale=W:-2,tile=GxG -frames:v K -f image2` spawn in
//! `pharos-cache::trickplay_cache`. Each filtered output frame is one
//! GxG sprite sheet; we write them 0-based as `{i}.jpg` into `out_dir`.

use super::frames::{self, FrameError};
use ffmpeg::format::Pixel;
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Generate up to `max_sheets` trickplay sprite sheets from `src`.
/// - `interval_ms`: spacing between sampled thumbnails.
/// - `width`: per-thumbnail width (aspect-preserved, even height).
/// - `grid`: GxG thumbnails per sheet.
/// - `thumb_count`: total thumbnails to sample (`ceil(duration/interval)`);
///   the sample timestamps are `0, interval, 2·interval, …`. Bounding by an
///   explicit count (rather than walking to EOF) is what lets the seek driver
///   stop cleanly at the end even when the last keyframe sits well before it.
/// - `quality`: FFmpeg `-q:v` (5 matches the spawn path).
///
/// Samples by **seeking** to each timestamp instead of demuxing the whole
/// file: on a long source over NFS the whole-file walk read every byte and
/// timed the worker out before any sprite landed. Returns the number of
/// sheets actually written (may be `< max_sheets` on short sources).
#[allow(clippy::too_many_arguments)]
pub fn trickplay_sprite(
    src: &Path,
    interval_ms: u64,
    width: u32,
    grid: u32,
    thumb_count: u32,
    max_sheets: u32,
    quality: i32,
    out_dir: &Path,
) -> Result<u32, FrameError> {
    if max_sheets == 0 {
        return Err(FrameError::Other("max_sheets = 0".into()));
    }
    std::fs::create_dir_all(out_dir)
        .map_err(|e| FrameError::Other(format!("mkdir {}: {e}", out_dir.display())))?;
    // No `fps` here — the seek driver already samples one frame per interval.
    let spec =
        format!("scale={width}:-2:flags=fast_bilinear,tile={grid}x{grid}:padding=0:margin=0");

    let interval_ms = interval_ms.max(1);
    let targets = (0..thumb_count).map(|i| i as u64 * interval_ms);

    let mut produced: u32 = 0;
    let mut write_err: Option<FrameError> = None;
    frames::filter_video_seeked(src, targets, &spec, Pixel::YUVJ420P, |f| {
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
