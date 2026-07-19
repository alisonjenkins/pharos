//! In-process subtitle event-window scan — the data behind per-segment
//! burn gating.
//!
//! Image-subtitle burn-in (PGS/VOBSUB overlay, B40/B44) costs a full
//! decode + composite + re-encode per HLS segment, which runs BELOW
//! realtime for VP9 (~6-11 s per 6 s segment observed live). But a forced
//! track (the Na'vi case) is SPARSE: most segments contain no subtitle
//! event at all, so paying the overlay pipeline for them is pure waste.
//! This scan reads the subtitle stream's PACKET timeline once (demux only,
//! no decode — bounded by NFS read throughput, not CPU) so the segment
//! layer can skip the burn for event-free windows.
//!
//! Window semantics: a packet at pts `t` with duration `d` covers
//! `[t, t+d]`. Packets without a duration (PGS often omits it — the
//! display set stays up until the next one, which may itself be an empty
//! "clear" set) extend to the NEXT packet's pts, capped at
//! [`MAX_UNDURATED_WINDOW_MS`]; the final packet gets the cap. Windows are
//! emitted sorted and merged when overlapping, so consumers do a simple
//! interval-overlap test.

use super::frames::FrameError;
use crate::subwin::{merge_windows, WindowMs};
use ffmpeg::media;
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Cap for a window whose packet carries no duration and has no successor
/// (or a far-away one): a PGS display set is never plausibly on screen
/// longer than this; without the cap a missing clear-event would mark the
/// rest of the file as "has subtitles" and disable gating entirely.
pub const MAX_UNDURATED_WINDOW_MS: u64 = 15_000;

/// Scan the packet timeline of the `stream_rel_idx`-th SUBTITLE stream
/// (codec-relative, same convention as `-map 0:s:N` / the burn filter's
/// `[0:s:N]`) and return merged on-screen windows in ms.
pub fn subtitle_event_windows(
    src: &Path,
    stream_rel_idx: u32,
) -> Result<Vec<WindowMs>, FrameError> {
    crate::libav::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    let mut ictx = format_input(src)?;

    // Resolve the codec-relative subtitle index to the absolute stream
    // index, and grab the stream's time base for pts → ms conversion.
    let mut abs_index: Option<usize> = None;
    let mut tb_num: i32 = 1;
    let mut tb_den: i32 = 1000;
    let mut rel = 0u32;
    for stream in ictx.streams() {
        if stream.parameters().medium() == media::Type::Subtitle {
            if rel == stream_rel_idx {
                abs_index = Some(stream.index());
                let tb = stream.time_base();
                tb_num = tb.numerator();
                tb_den = tb.denominator();
                break;
            }
            rel += 1;
        }
    }
    let abs_index = abs_index
        .ok_or_else(|| FrameError::BadInput(format!("no subtitle stream s:{stream_rel_idx}")))?;

    let to_ms = |ts: i64| -> u64 {
        if ts <= 0 || tb_den == 0 {
            return 0;
        }
        ((ts as i128 * tb_num as i128 * 1000) / tb_den as i128).max(0) as u64
    };

    // (start_ms, duration_ms if the packet carried one)
    let mut raw: Vec<(u64, Option<u64>)> = Vec::new();
    for res in ictx.packets() {
        let (stream, packet) = match res {
            Ok(sp) => sp,
            // A damaged packet mid-file shouldn't void the whole scan.
            Err(_) => continue,
        };
        if stream.index() != abs_index {
            continue;
        }
        let Some(pts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        let dur = packet.duration();
        raw.push((to_ms(pts), (dur > 0).then(|| to_ms(dur))));
    }
    raw.sort_unstable_by_key(|(s, _)| *s);

    // Close undurated windows at the next packet (a PGS clear-set ends the
    // previous display set), capped so a missing clear never floods the
    // rest of the timeline.
    let mut windows: Vec<WindowMs> = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let (start, dur) = raw[i];
        let end = match dur {
            Some(d) => start + d.min(MAX_UNDURATED_WINDOW_MS),
            None => {
                let next = raw.get(i + 1).map(|(s, _)| *s);
                match next {
                    Some(n) if n > start => n.min(start + MAX_UNDURATED_WINDOW_MS),
                    _ => start + MAX_UNDURATED_WINDOW_MS,
                }
            }
        };
        // A zero-length window is a clear-event; it opens nothing.
        if end > start {
            windows.push((start, end));
        }
    }
    Ok(merge_windows(windows))
}

fn format_input(src: &Path) -> Result<ffmpeg::format::context::Input, FrameError> {
    ffmpeg::format::input(src).map_err(|e| FrameError::BadInput(format!("open: {e}")))
}
