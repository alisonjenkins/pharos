//! In-process trickplay sprite-sheet generation — replaces the `ffmpeg
//! -vf fps=1/N,scale=W:-2,tile=GxG -frames:v K -f image2` spawn in
//! `pharos-cache::trickplay_cache`. Each filtered output frame is one
//! GxG sprite sheet; we write them 0-based as `{i}.jpg` into `out_dir`.

use super::frames::{self, FrameError};
use ffmpeg::format::Pixel;
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Build the filter spec for trickplay sprite sheets, with optional left-eye crop for packed 3D.
pub fn trickplay_filter_spec(
    width: u32,
    grid: u32,
    video_3d_format: Option<pharos_core::Video3DFormat>,
) -> String {
    let scale_tile =
        format!("scale={width}:-2:flags=fast_bilinear,tile={grid}x{grid}:padding=0:margin=0");
    match super::image::stereo_left_eye_filter_prefix(video_3d_format) {
        Some(prefix) => format!("{prefix},{scale_tile}"),
        None => scale_tile,
    }
}

/// Generate up to `max_sheets` trickplay sprite sheets from `src`, with optional packed 3D left-eye crop.
#[allow(clippy::too_many_arguments)]
pub fn trickplay_sprite_3d(
    src: &Path,
    interval_ms: u64,
    width: u32,
    grid: u32,
    thumb_count: u32,
    max_sheets: u32,
    quality: i32,
    video_3d_format: Option<pharos_core::Video3DFormat>,
    out_dir: &Path,
) -> Result<u32, FrameError> {
    if max_sheets == 0 {
        return Err(FrameError::Other("max_sheets = 0".into()));
    }
    std::fs::create_dir_all(out_dir)
        .map_err(|e| FrameError::Other(format!("mkdir {}: {e}", out_dir.display())))?;
    let spec = trickplay_filter_spec(width, grid, video_3d_format);

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
    trickplay_sprite_3d(
        src,
        interval_ms,
        width,
        grid,
        thumb_count,
        max_sheets,
        quality,
        None,
        out_dir,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ffmpeg_the_third as ffmpeg;
    use pharos_core::Video3DFormat;

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
    fn trickplay_filter_spec_formats() {
        assert_eq!(
            trickplay_filter_spec(320, 5, Some(Video3DFormat::HalfSideBySide)),
            "crop=iw/2:ih:0:0,scale=iw*2:ih,setsar=1,scale=320:-2:flags=fast_bilinear,tile=5x5:padding=0:margin=0"
        );
        assert_eq!(
            trickplay_filter_spec(320, 5, Some(Video3DFormat::FullSideBySide)),
            "crop=iw/2:ih:0:0,setsar=1,scale=320:-2:flags=fast_bilinear,tile=5x5:padding=0:margin=0"
        );
        assert_eq!(
            trickplay_filter_spec(320, 5, Some(Video3DFormat::HalfTopAndBottom)),
            "crop=iw:ih/2:0:0,scale=iw:ih*2,setsar=1,scale=320:-2:flags=fast_bilinear,tile=5x5:padding=0:margin=0"
        );
        assert_eq!(
            trickplay_filter_spec(320, 5, Some(Video3DFormat::FullTopAndBottom)),
            "crop=iw:ih/2:0:0,setsar=1,scale=320:-2:flags=fast_bilinear,tile=5x5:padding=0:margin=0"
        );
        assert_eq!(
            trickplay_filter_spec(320, 5, Some(Video3DFormat::Mvc)),
            "scale=320:-2:flags=fast_bilinear,tile=5x5:padding=0:margin=0"
        );
        assert_eq!(
            trickplay_filter_spec(320, 5, None),
            "scale=320:-2:flags=fast_bilinear,tile=5x5:padding=0:margin=0"
        );
    }

    #[test]
    fn left_eye_crop_trickplay_tile_synthetic_frames() {
        use super::super::image::mean_luma;

        // SBS frame: left half 200, right half 20.
        let sbs = sbs_frame(64, 64, 200, 20);
        let uncropped_spec = trickplay_filter_spec(32, 1, None);
        let uncropped = run_filter(&sbs, &uncropped_spec);
        let uncropped_luma = mean_luma(&uncropped);
        assert!(
            uncropped_luma >= 105 && uncropped_luma <= 115,
            "uncropped: {uncropped_luma}"
        );

        let hsbs_spec = trickplay_filter_spec(32, 1, Some(Video3DFormat::HalfSideBySide));
        let hsbs_out = run_filter(&sbs, &hsbs_spec);
        let hsbs_luma = mean_luma(&hsbs_out);
        assert!(
            hsbs_luma >= 195 && hsbs_luma <= 205,
            "expected ~200, got {hsbs_luma}"
        );

        let fsbs_spec = trickplay_filter_spec(32, 1, Some(Video3DFormat::FullSideBySide));
        let fsbs_out = run_filter(&sbs, &fsbs_spec);
        let fsbs_luma = mean_luma(&fsbs_out);
        assert!(
            fsbs_luma >= 195 && fsbs_luma <= 205,
            "expected ~200, got {fsbs_luma}"
        );

        // TAB frame: top half 200, bottom half 20.
        let tab = tab_frame(64, 64, 200, 20);
        let uncropped_tab_spec = trickplay_filter_spec(32, 1, None);
        let uncropped_tab = run_filter(&tab, &uncropped_tab_spec);
        let uncropped_tab_luma = mean_luma(&uncropped_tab);
        assert!(
            uncropped_tab_luma >= 105 && uncropped_tab_luma <= 115,
            "uncropped tab: {uncropped_tab_luma}"
        );

        let htab_spec = trickplay_filter_spec(32, 1, Some(Video3DFormat::HalfTopAndBottom));
        let htab_out = run_filter(&tab, &htab_spec);
        let htab_luma = mean_luma(&htab_out);
        assert!(
            htab_luma >= 195 && htab_luma <= 205,
            "expected ~200, got {htab_luma}"
        );

        let ftab_spec = trickplay_filter_spec(32, 1, Some(Video3DFormat::FullTopAndBottom));
        let ftab_out = run_filter(&tab, &ftab_spec);
        let ftab_luma = mean_luma(&ftab_out);
        assert!(
            ftab_luma >= 195 && ftab_luma <= 205,
            "expected ~200, got {ftab_luma}"
        );
    }
}
