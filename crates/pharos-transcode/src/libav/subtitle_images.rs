//! In-process image-subtitle extraction for the native player (spec 019,
//! ADR-0060).
//!
//! Decodes every event of one bitmap subtitle stream (DVD `dvd_subtitle`,
//! Blu-ray `hdmv_pgs_subtitle`, DVB `dvb_subtitle`), applies each rect's
//! palette, and writes it to `out_dir` as `{n}.png` (RGBA), with a
//! [`ImageSubManifest`] in `manifest.json`. The manifest is rewritten as the
//! decode advances (`complete: false`, `through_ms`) so a player can show the
//! early cues of a cold file while the rest of it is still being read.
//!
//! Timing (FR-005): an event shows from its packet pts plus
//! `start_display_time`. It ends at `end_display_time` when the decoder gives
//! one; otherwise at the next event's start (a PGS display set is cleared by
//! an empty one), capped at [`MAX_UNDURATED_WINDOW_MS`].

use super::frames::FrameError;
use super::subtitle_windows::MAX_UNDURATED_WINDOW_MS;
use ffmpeg::{codec, format, frame, media, Rational};
use ffmpeg_the_third as ffmpeg;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};

/// The manifest's file name inside the output directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// How often the partial manifest is rewritten while decoding: often enough
/// that a viewer of a cold file sees cues within seconds, rare enough that
/// rewriting it costs nothing next to the demux.
const PROGRESS_EVERY: Duration = Duration::from_secs(2);

/// Packet read errors in a row after which the source counts as unreadable:
/// a damaged packet is skipped, but a dead mount returns the same error on
/// every read and would otherwise never reach the end.
const MAX_READ_ERRORS_IN_A_ROW: u32 = 64;

/// The longest a timed event is shown: a corrupt or absurd end time must not
/// keep a bitmap on screen for minutes. Generous next to the undurated cap,
/// because a stated duration is usually right.
const MAX_EVENT_MS: u64 = 60_000;

/// One bitmap on screen: where it sits on the subtitle canvas and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSubEvent {
    pub start_ms: u64,
    pub end_ms: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub forced: bool,
    /// The PNG's file name in the output directory.
    pub file: String,
}

/// Every event decoded so far, on a canvas of `width` x `height` (the
/// coordinate space of the events' rects).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSubManifest {
    pub width: u32,
    pub height: u32,
    /// The whole stream has been read.
    pub complete: bool,
    /// The source position the decode has reached: events up to here are
    /// all in `events`.
    pub through_ms: u64,
    /// Subtitle packets the decoder refused; their events are missing.
    #[serde(default)]
    pub decode_errors: u64,
    pub events: Vec<ImageSubEvent>,
}

/// Extract the `stream_rel_idx`-th subtitle stream (codec-relative, the `N`
/// of `-map 0:s:N`) of `src` into `out_dir`, returning the final manifest.
pub fn extract_subtitle_images(
    src: &Path,
    stream_rel_idx: u32,
    out_dir: &Path,
) -> Result<ImageSubManifest, FrameError> {
    crate::libav::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    std::fs::create_dir_all(out_dir)
        .map_err(|e| FrameError::Other(format!("create {}: {e}", out_dir.display())))?;
    let mut ictx = format::input(src).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;

    let mut video_size = (0u32, 0u32);
    let mut found = None;
    let mut rel = 0u32;
    for stream in ictx.streams() {
        match stream.parameters().medium() {
            media::Type::Video if video_size == (0, 0) => {
                // SAFETY: codecpar is valid for the stream's lifetime.
                let par = unsafe { &*(*stream.as_ptr()).codecpar };
                video_size = (
                    u32::try_from(par.width).unwrap_or(0),
                    u32::try_from(par.height).unwrap_or(0),
                );
            }
            media::Type::Subtitle => {
                if rel == stream_rel_idx && found.is_none() {
                    let decoder = codec::context::Context::from_parameters(stream.parameters())
                        .and_then(|c| c.decoder().subtitle())
                        .map_err(|e| FrameError::BadInput(format!("subtitle decoder: {e}")))?;
                    found = Some((stream.index(), stream.time_base(), decoder));
                }
                rel += 1;
            }
            _ => {}
        }
    }
    let (abs_index, time_base, mut decoder) = found
        .ok_or_else(|| FrameError::BadInput(format!("no subtitle stream s:{stream_rel_idx}")))?;
    // The player's clock starts at the container's first timestamp, which a
    // TS/m2ts/PS source does not put at zero.
    // SAFETY: the format context outlives this read.
    let origin_ms = match unsafe { (*ictx.as_ptr()).start_time } {
        t if t == ffmpeg::ffi::AV_NOPTS_VALUE || t <= 0 => 0,
        t => u64::try_from(t / 1000).unwrap_or(0),
    };

    let mut out = Extraction {
        out_dir,
        manifest: ImageSubManifest::default(),
        showing: Vec::new(),
        last_write: Instant::now(),
    };
    let mut read_errors = 0;
    let mut last_decode_error = None;
    for res in ictx.packets() {
        // A damaged packet mid-file should not void the whole extraction.
        let (stream, packet) = match res {
            Ok(read) => {
                read_errors = 0;
                read
            }
            Err(e) => {
                read_errors += 1;
                if read_errors >= MAX_READ_ERRORS_IN_A_ROW {
                    return Err(FrameError::BadInput(format!(
                        "{read_errors} packet reads in a row failed, last: {e}"
                    )));
                }
                continue;
            }
        };
        if stream.index() != abs_index {
            continue;
        }
        let Some(pts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        let at_ms = ts_to_ms(pts, time_base).saturating_sub(origin_ms);
        let mut sub = OwnedSubtitle::new();
        match decoder.decode(&packet, &mut sub.0) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                out.manifest.decode_errors += 1;
                last_decode_error = Some(format!("at {at_ms} ms: {e}"));
                continue;
            }
        }
        if out.manifest.width == 0 {
            // SAFETY: the decoder context outlives this read.
            let ctx = unsafe { &*decoder.as_ptr() };
            let canvas = (
                u32::try_from(ctx.width).unwrap_or(0),
                u32::try_from(ctx.height).unwrap_or(0),
            );
            let (w, h) = if canvas.0 > 0 && canvas.1 > 0 {
                canvas
            } else {
                video_size
            };
            out.manifest.width = w;
            out.manifest.height = h;
        }
        out.add(&sub.0, at_ms)?;
        out.manifest.through_ms = out.manifest.through_ms.max(at_ms);
        if out.last_write.elapsed() >= PROGRESS_EVERY {
            out.write()?;
        }
    }
    out.close_showing(None);
    decoded_anything(&out.manifest, last_decode_error.as_deref())?;
    out.manifest.complete = true;
    out.write()?;
    Ok(out.manifest)
}

/// A track whose every packet failed to decode is a failed extraction, not
/// an empty one; a few failures among good events are recorded in the
/// manifest's `decode_errors`.
fn decoded_anything(m: &ImageSubManifest, last_error: Option<&str>) -> Result<(), FrameError> {
    if m.events.is_empty() && m.decode_errors > 0 {
        return Err(FrameError::BadInput(format!(
            "all {} subtitle packets failed to decode, last {}",
            m.decode_errors,
            last_error.unwrap_or("unknown")
        )));
    }
    Ok(())
}

struct Extraction<'a> {
    out_dir: &'a Path,
    manifest: ImageSubManifest,
    /// The events of the last display set, and whether each stated its own
    /// end; the next display set ends them all.
    showing: Vec<(usize, bool)>,
    last_write: Instant,
}

impl Extraction<'_> {
    fn add(&mut self, sub: &ffmpeg::Subtitle, at_ms: u64) -> Result<(), FrameError> {
        let start = at_ms + u64::from(sub.start());
        // A new display set (or an empty clear set) ends what was showing.
        self.close_showing(Some(start));
        let end = (sub.end() > sub.start() && sub.end() != u32::MAX)
            .then(|| (at_ms + u64::from(sub.end())).min(start + MAX_EVENT_MS));
        for rect in sub.rects() {
            let Some(bitmap) = Bitmap::of(&rect) else {
                continue;
            };
            let n = self.manifest.events.len();
            let file = format!("{n}.png");
            let png = bitmap.to_png()?;
            // Served and cached as soon as the manifest names it, so it must
            // never be seen half-written.
            let tmp = self.out_dir.join(format!("{file}.tmp"));
            std::fs::write(&tmp, png)
                .and_then(|()| std::fs::rename(&tmp, self.out_dir.join(&file)))
                .map_err(|e| FrameError::Other(format!("write {file}: {e}")))?;
            self.showing.push((n, end.is_some()));
            self.manifest.events.push(ImageSubEvent {
                start_ms: start,
                end_ms: end.unwrap_or(start + MAX_UNDURATED_WINDOW_MS),
                x: bitmap.x,
                y: bitmap.y,
                width: bitmap.width,
                height: bitmap.height,
                forced: bitmap.forced,
                file,
            });
        }
        Ok(())
    }

    /// End what is showing at `at_ms`, the next display set's start: a timed
    /// event at whichever comes first, an undurated one there or at its cap.
    /// At the end of the stream (`None`) an undurated event takes its cap.
    fn close_showing(&mut self, at_ms: Option<u64>) {
        for (n, timed) in self.showing.drain(..) {
            let e = &mut self.manifest.events[n];
            let end = if timed {
                e.end_ms
            } else {
                e.start_ms + MAX_UNDURATED_WINDOW_MS
            };
            e.end_ms = at_ms
                .filter(|&t| t >= e.start_ms)
                .map_or(end, |t| t.min(end));
        }
    }

    /// Replace `manifest.json` atomically, so a reader never sees half of it.
    fn write(&mut self) -> Result<(), FrameError> {
        let body = serde_json::to_vec(&self.manifest)
            .map_err(|e| FrameError::Other(format!("manifest json: {e}")))?;
        let tmp = self.out_dir.join(format!("{MANIFEST_FILE}.tmp"));
        std::fs::write(&tmp, body)
            .and_then(|()| std::fs::rename(&tmp, self.out_dir.join(MANIFEST_FILE)))
            .map_err(|e| FrameError::Other(format!("write manifest: {e}")))?;
        self.last_write = Instant::now();
        Ok(())
    }
}

/// An `AVSubtitle` freed on drop (`ffmpeg_the_third::Subtitle` does not).
struct OwnedSubtitle(ffmpeg::Subtitle);

impl OwnedSubtitle {
    fn new() -> Self {
        Self(ffmpeg::Subtitle::new())
    }
}

impl Drop for OwnedSubtitle {
    fn drop(&mut self) {
        // SAFETY: the subtitle was zeroed or filled by the decoder; freeing a
        // zeroed one is a no-op.
        unsafe { ffmpeg::ffi::avsubtitle_free(self.0.as_mut_ptr()) };
    }
}

/// A rect's palette-applied pixels, RGBA, rows packed.
struct Bitmap {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    forced: bool,
    rgba: Vec<u8>,
}

impl Bitmap {
    fn of(rect: &ffmpeg::codec::subtitle::RectRef<'_>) -> Option<Self> {
        if rect.kind() != ffmpeg::codec::subtitle::Type::Bitmap {
            return None;
        }
        let (width, height) = (rect.width(), rect.height());
        if width == 0 || height == 0 {
            return None;
        }
        // SAFETY: a bitmap rect holds `height` rows of `linesize[0]` palette
        // indices in data[0] and `nb_colors` AARRGGBB entries in data[1].
        let raw = unsafe { &*ffmpeg::AsPtr::as_ptr(rect) };
        let stride = usize::try_from(raw.linesize[0]).ok()?;
        let colors = usize::try_from(raw.nb_colors).ok()?;
        if raw.data[0].is_null() || raw.data[1].is_null() || stride < width as usize {
            return None;
        }
        let (w, h) = (width as usize, height as usize);
        let indices = unsafe { std::slice::from_raw_parts(raw.data[0], stride * h) };
        let palette =
            unsafe { std::slice::from_raw_parts(raw.data[1].cast::<u32>(), colors.min(256)) };
        let rgba = apply_palette(indices, stride, w, h, palette);
        Some(Self {
            x: u32::try_from(rect.x()).unwrap_or(0),
            y: u32::try_from(rect.y()).unwrap_or(0),
            width,
            height,
            forced: rect
                .flags()
                .contains(ffmpeg::codec::subtitle::Flags::FORCED),
            rgba,
        })
    }

    fn to_png(&self) -> Result<Vec<u8>, FrameError> {
        let mut frame = frame::Video::new(format::Pixel::RGBA, self.width, self.height);
        let stride = frame.stride(0);
        let row = self.width as usize * 4;
        let data = frame.data_mut(0);
        for (y, src) in self.rgba.chunks(row).enumerate() {
            data[y * stride..y * stride + row].copy_from_slice(src);
        }
        let enc_codec = ffmpeg::encoder::find(codec::Id::PNG)
            .ok_or_else(|| FrameError::Other("no png encoder".into()))?;
        let mut enc = codec::context::Context::new_with_codec(enc_codec)
            .encoder()
            .video()
            .map_err(|e| FrameError::Other(format!("png enc: {e}")))?;
        enc.set_width(self.width);
        enc.set_height(self.height);
        enc.set_format(format::Pixel::RGBA);
        enc.set_time_base(Rational(1, 25));
        let mut opened = enc
            .open()
            .map_err(|e| FrameError::Other(format!("open png: {e}")))?;
        opened
            .send_frame(&frame)
            .and_then(|()| opened.send_eof())
            .map_err(|e| FrameError::Other(format!("png send: {e}")))?;
        let mut out = Vec::new();
        let mut packet = codec::packet::Packet::empty();
        while opened.receive_packet(&mut packet).is_ok() {
            if let Some(data) = packet.data() {
                out.extend_from_slice(data);
            }
        }
        if out.is_empty() {
            return Err(FrameError::Other("png produced no data".into()));
        }
        Ok(out)
    }
}

/// Map `h` rows of `w` palette indices (rows `stride` apart) through a
/// palette of AARRGGBB entries to packed RGBA. An index past the palette is
/// transparent.
fn apply_palette(indices: &[u8], stride: usize, w: usize, h: usize, palette: &[u32]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in indices.chunks(stride).take(h) {
        for &i in &row[..w] {
            let argb = palette.get(usize::from(i)).copied().unwrap_or(0);
            let [a, r, g, b] = argb.to_be_bytes();
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    rgba
}

fn ts_to_ms(ts: i64, tb: Rational) -> u64 {
    if ts <= 0 || tb.denominator() == 0 {
        return 0;
    }
    let ms = i128::from(ts) * i128::from(tb.numerator()) * 1000 / i128::from(tb.denominator());
    u64::try_from(ms).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(events: Vec<(u64, u64)>) -> Extraction<'static> {
        Extraction {
            out_dir: Path::new("/nonexistent"),
            manifest: ImageSubManifest {
                events: events
                    .into_iter()
                    .map(|(start_ms, end_ms)| ImageSubEvent {
                        start_ms,
                        end_ms,
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                        forced: false,
                        file: String::new(),
                    })
                    .collect(),
                ..ImageSubManifest::default()
            },
            showing: Vec::new(),
            last_write: Instant::now(),
        }
    }

    #[test]
    fn a_timed_event_ends_where_the_next_display_set_starts() {
        let mut x = manifest_with(vec![(1_000, 9_000)]);
        x.showing.push((0, true));
        x.close_showing(Some(4_000));
        assert_eq!(x.manifest.events[0].end_ms, 4_000);
    }

    #[test]
    fn a_timed_event_keeps_its_end_when_the_next_set_comes_later() {
        let mut x = manifest_with(vec![(1_000, 9_000)]);
        x.showing.push((0, true));
        x.close_showing(Some(20_000));
        assert_eq!(x.manifest.events[0].end_ms, 9_000);
        let mut x = manifest_with(vec![(1_000, 9_000)]);
        x.showing.push((0, true));
        x.close_showing(None);
        assert_eq!(x.manifest.events[0].end_ms, 9_000);
    }

    #[test]
    fn a_track_where_every_packet_failed_to_decode_is_an_error() {
        let failed = ImageSubManifest {
            decode_errors: 3,
            ..ImageSubManifest::default()
        };
        assert!(decoded_anything(&failed, Some("at 5 ms: Invalid data")).is_err());
        assert!(decoded_anything(&ImageSubManifest::default(), None).is_ok());
        let some = manifest_with(vec![(0, 1)]).manifest;
        let partly = ImageSubManifest {
            decode_errors: 3,
            ..some
        };
        assert!(decoded_anything(&partly, None).is_ok());
    }

    #[test]
    fn an_open_event_ends_where_the_next_display_set_starts() {
        let mut x = manifest_with(vec![(1_000, 16_000)]);
        x.showing.push((0, false));
        x.close_showing(Some(3_500));
        assert_eq!(x.manifest.events[0].end_ms, 3_500);
        assert!(x.showing.is_empty());
    }

    #[test]
    fn a_display_set_at_the_same_time_replaces_the_open_one() {
        let mut x = manifest_with(vec![(1_000, 0)]);
        x.showing.push((0, false));
        x.close_showing(Some(1_000));
        assert_eq!(x.manifest.events[0].end_ms, 1_000);
    }

    #[test]
    fn an_open_event_with_no_successor_ends_at_the_cap() {
        let mut x = manifest_with(vec![(1_000, 0)]);
        x.showing.push((0, false));
        x.close_showing(None);
        assert_eq!(x.manifest.events[0].end_ms, 1_000 + MAX_UNDURATED_WINDOW_MS);
    }

    #[test]
    fn a_successor_far_away_does_not_stretch_an_event_past_the_cap() {
        let mut x = manifest_with(vec![(1_000, 0)]);
        x.showing.push((0, false));
        x.close_showing(Some(60_000));
        assert_eq!(x.manifest.events[0].end_ms, 1_000 + MAX_UNDURATED_WINDOW_MS);
    }

    #[test]
    fn the_palette_maps_indices_to_rgba_and_skips_the_row_padding() {
        // 2x2 bitmap, rows padded to 3 bytes; index 2 is past the palette.
        let indices = [0, 1, 9, 1, 2, 9];
        let palette = [0x0000_0000, 0xFFFF_FF00];
        let rgba = apply_palette(&indices, 3, 2, 2, &palette);
        assert_eq!(
            rgba,
            [
                0, 0, 0, 0, 255, 255, 0, 255, //
                255, 255, 0, 255, 0, 0, 0, 0,
            ]
        );
    }

    #[test]
    fn timestamps_convert_through_the_stream_time_base() {
        assert_eq!(ts_to_ms(90_000, Rational(1, 90_000)), 1_000);
        assert_eq!(ts_to_ms(505, Rational(1, 1000)), 505);
        assert_eq!(ts_to_ms(-5, Rational(1, 1000)), 0);
    }
}
