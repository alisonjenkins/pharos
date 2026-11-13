//! Keyframe index and packet copy for the H.264 copy surface (spec 041,
//! ADR-0123).
//!
//! [`index`] reads a source's seek index, plans segments on its keyframes and
//! then reads and writes the first [`VALIDATED_SEGMENTS`] in full, so a file
//! whose packets disagree with its index, or whose GOPs are open, is refused
//! before a player asks for anything. [`copy_segment`] seeks to one planned
//! keyframe and writes every video packet up to the next one, unchanged.
//!
//! Matroska only for now: its cue times are presentation times. An MP4's index
//! holds decode times, which differ from the keyframes' presentation times
//! whenever the file has B-frames.

use super::frames::FrameError;
use crate::copy_fmp4::{self, CopyPacket, CopyTrack};
use crate::copy_surface::{
    CopyIndexVerdict, CopyRefusal, CopySegmentReport, CopySource, VALIDATED_SEGMENTS,
};
use ffmpeg::codec::field_order::FieldOrder;
use ffmpeg::codec::Id;
use ffmpeg_the_third as ffmpeg;
use pharos_core::copy_plan::{self, CopyPlan, CopySegment};
use std::path::Path;

/// Packets one segment may hold: 12 s at 240 fps. More means the plan and the
/// file disagree, and reading on would only hold memory.
const MAX_SEGMENT_PACKETS: usize = 12 * 240;
/// Video packets read after a seek before the segment's keyframe must appear.
/// A seek lands on the cue at or before the target; cues are at most a few
/// GOPs apart.
const MAX_SKIPPED_PACKETS: usize = 2 * MAX_SEGMENT_PACKETS;
/// `AVINDEX_KEYFRAME` from libavformat.
const INDEX_KEYFRAME: i32 = 1;

struct Opened {
    ictx: ffmpeg::format::context::Input,
    stream_index: usize,
    track: CopyTrack,
    origin: i64,
}

/// Index `src` for the copy surface. `Err` only when libav cannot open the
/// file at all; every reason the file cannot be copied is a `Refused` verdict.
pub fn index(src: &Path) -> Result<CopyIndexVerdict, FrameError> {
    let mut opened = match open(src)? {
        Ok(o) => o,
        Err(r) => return Ok(CopyIndexVerdict::Refused(r)),
    };
    let timescale = opened.track.timescale;
    let container_end = match duration_ticks(&opened.ictx, timescale) {
        Some(e) => e,
        None => return Ok(CopyIndexVerdict::Refused(CopyRefusal::NoDuration)),
    };
    // Matroska reads cues stored after the clusters on its first seek.
    let _ = opened.ictx.seek(0, ..=0);
    let keyframes = index_keyframes(&opened, timescale);
    let Some(&first_keyframe) = keyframes.first() else {
        return Ok(CopyIndexVerdict::Refused(CopyRefusal::NoIndex));
    };
    // Segment 0 starts at the first indexed keyframe, so any packet before it
    // would belong to no segment.
    match read_video(&mut opened) {
        Ok(Some(first)) if first.keyframe && first.pts == first_keyframe => {}
        Ok(Some(first)) => {
            return Ok(CopyIndexVerdict::Refused(CopyRefusal::LeadingPackets {
                pts: first.pts,
                keyframe: first.keyframe,
                first_keyframe,
            }));
        }
        Ok(None) => return Ok(CopyIndexVerdict::Refused(CopyRefusal::NoIndex)),
        Err(detail) => {
            return Ok(CopyIndexVerdict::Refused(CopyRefusal::Segment {
                seg: 0,
                detail,
            }));
        }
    }
    let mut plan = match copy_plan::plan(&keyframes, container_end, timescale) {
        Ok(p) => p,
        Err(e) => return Ok(CopyIndexVerdict::Refused(CopyRefusal::Plan(e))),
    };
    // The playlist advertises the plan's durations, so the plan's end has to
    // cover the last packet even when the container understates its duration.
    let tail_end = match last_segment_end(&mut opened, &plan) {
        Ok(e) => e,
        Err((seg, detail)) => {
            return Ok(CopyIndexVerdict::Refused(CopyRefusal::Segment {
                seg,
                detail,
            }));
        }
    };
    if tail_end > container_end {
        plan = match copy_plan::plan(&keyframes, tail_end, timescale) {
            Ok(p) => p,
            Err(e) => return Ok(CopyIndexVerdict::Refused(CopyRefusal::Plan(e))),
        };
    }

    seek_to(&mut opened, plan.segments.first().map_or(0, |s| s.start))
        .map_err(FrameError::BadInput)?;
    let mut pending = None;
    let count = plan.segments.len();
    for (i, seg) in plan.segments.iter().take(VALIDATED_SEGMENTS).enumerate() {
        let last = i + 1 == count;
        let checked = collect(&mut opened, *seg, last, &mut pending)
            .and_then(|packets| write(&opened.track, i, *seg, last, &packets).map(|_| ()));
        if let Err(detail) = checked {
            return Ok(CopyIndexVerdict::Refused(CopyRefusal::Segment {
                seg: i,
                detail,
            }));
        }
    }
    let Opened { track, origin, .. } = opened;
    Ok(CopyIndexVerdict::Copyable(Box::new(CopySource {
        track,
        plan,
        origin,
    })))
}

/// Write segment `seg` (index `index` in the plan, `last` when it is the
/// plan's final one) of `src` to `out` as an fMP4 media segment.
pub fn copy_segment(
    src: &Path,
    index: usize,
    seg: CopySegment,
    last: bool,
    out: &Path,
) -> Result<CopySegmentReport, FrameError> {
    let mut opened = match open(src)? {
        Ok(o) => o,
        Err(r) => return Err(FrameError::BadInput(r.to_string())),
    };
    seek_to(&mut opened, seg.start).map_err(FrameError::BadInput)?;
    let mut pending = None;
    let packets = collect(&mut opened, seg, last, &mut pending)
        .map_err(|d| FrameError::BadInput(format!("segment {index}: {d}")))?;
    let bytes = write(&opened.track, index, seg, last, &packets)
        .map_err(|d| FrameError::BadInput(format!("segment {index}: {d}")))?;
    std::fs::write(out, &bytes)
        .map_err(|e| FrameError::Other(format!("write {}: {e}", out.display())))?;
    Ok(CopySegmentReport {
        packets: u32::try_from(packets.len()).unwrap_or(u32::MAX),
        bytes: bytes.len() as u64,
    })
}

fn open(src: &Path) -> Result<Result<Opened, CopyRefusal>, FrameError> {
    super::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    let ictx =
        ffmpeg::format::input(src).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;
    let container = ictx.format().name().to_owned();
    if !container.split(',').any(|n| n == "matroska") {
        return Ok(Err(CopyRefusal::Container(container)));
    }
    let Some(stream) = super::probe::primary_video_stream(&ictx) else {
        return Ok(Err(CopyRefusal::NoVideo));
    };
    let stream_index = stream.index();
    let tb = stream.time_base();
    let params = stream.parameters();
    if params.id() != Id::H264 {
        return Ok(Err(CopyRefusal::Codec(format!("{:?}", params.id()))));
    }
    if tb.numerator() != 1 || tb.denominator() <= 0 {
        return Ok(Err(CopyRefusal::TimeBase {
            num: tb.numerator(),
            den: tb.denominator(),
        }));
    }
    let timescale = tb.denominator().unsigned_abs();
    // SAFETY: `params` borrows the stream of the live `ictx`; the extradata
    // pointer and size are read together and copied out at once.
    let (avcc, pix_fmt) = unsafe {
        let p = params.as_ptr();
        let size = usize::try_from((*p).extradata_size).unwrap_or(0);
        let data = if (*p).extradata.is_null() || size == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts((*p).extradata, size).to_vec()
        };
        (data, (*p).format)
    };
    if avcc.first() != Some(&1) || avcc.len() < 7 {
        return Ok(Err(CopyRefusal::NotAvcc {
            first: avcc.first().copied(),
            len: avcc.len(),
        }));
    }
    let profile = avcc[1];
    if !matches!(profile, 66 | 77 | 100) {
        return Ok(Err(CopyRefusal::Profile(profile)));
    }
    if pix_fmt != ffmpeg::ffi::AVPixelFormat::YUV420P.0 {
        let name = ffmpeg::format::Pixel::from(ffmpeg::ffi::AVPixelFormat(pix_fmt));
        return Ok(Err(CopyRefusal::PixelFormat(format!("{name:?}"))));
    }
    let field_order = params.field_order();
    if !matches!(field_order, FieldOrder::Progressive | FieldOrder::Unknown) {
        return Ok(Err(CopyRefusal::Interlaced(format!("{field_order:?}"))));
    }
    let sar = params.sample_aspect_ratio();
    let sar = (sar.numerator() > 0 && sar.denominator() > 0).then(|| {
        (
            sar.numerator().unsigned_abs(),
            sar.denominator().unsigned_abs(),
        )
    });
    let width = u16::try_from(params.width()).unwrap_or(u16::MAX);
    let height = u16::try_from(params.height()).unwrap_or(u16::MAX);
    // SAFETY: reading a plain field of the live format context.
    let start_us = unsafe { (*ictx.as_ptr()).start_time };
    let origin = if start_us == ffmpeg::ffi::AV_NOPTS_VALUE {
        0
    } else {
        us_to_ticks(start_us, timescale)
    };
    Ok(Ok(Opened {
        ictx,
        stream_index,
        track: CopyTrack {
            avcc,
            width,
            height,
            sar,
            timescale,
        },
        origin,
    }))
}

fn us_to_ticks(us: i64, timescale: u32) -> i64 {
    let t = i128::from(us) * i128::from(timescale) / 1_000_000;
    i64::try_from(t).unwrap_or(i64::MAX)
}

fn duration_ticks(ictx: &ffmpeg::format::context::Input, timescale: u32) -> Option<i64> {
    let us = ictx.duration();
    (us > 0 && us != ffmpeg::ffi::AV_NOPTS_VALUE).then(|| us_to_ticks(us, timescale))
}

/// The video stream's keyframe cue times, relative to the origin, ascending
/// and distinct.
fn index_keyframes(opened: &Opened, _timescale: u32) -> Vec<i64> {
    let Some(stream) = opened.ictx.stream(opened.stream_index) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // SAFETY: the stream belongs to the live `ictx`; each entry is read at
    // once, before anything else can touch the index.
    unsafe {
        let st = stream.as_ptr();
        let n = ffmpeg::ffi::avformat_index_get_entries_count(st);
        for i in 0..n {
            let e = ffmpeg::ffi::avformat_index_get_entry(st as *mut _, i);
            if e.is_null() || (*e).flags() & INDEX_KEYFRAME == 0 {
                continue;
            }
            out.push((*e).timestamp - opened.origin);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn seek_to(opened: &mut Opened, start: i64) -> Result<(), String> {
    let ticks = start.saturating_add(opened.origin);
    let us = i128::from(ticks) * 1_000_000 / i128::from(opened.track.timescale);
    let us = i64::try_from(us).unwrap_or(i64::MAX);
    opened
        .ictx
        .seek(us, ..=us)
        .map_err(|e| format!("seek to {start} ticks: {e}"))
}

/// The next video packet, its times relative to the origin. `None` at end of
/// file; a read error ends the segment as a fault.
fn read_video(opened: &mut Opened) -> Result<Option<CopyPacket>, String> {
    loop {
        let Some(res) = opened.ictx.packets().next() else {
            return Ok(None);
        };
        let (stream, packet) = res.map_err(|e| format!("read: {e}"))?;
        if stream.index() != opened.stream_index {
            continue;
        }
        let pts = packet
            .pts()
            .ok_or_else(|| format!("packet with no pts after {:?}", packet.dts()))?;
        return Ok(Some(CopyPacket {
            data: packet.data().map(<[u8]>::to_vec).unwrap_or_default(),
            pts: pts - opened.origin,
            keyframe: packet.is_key(),
        }));
    }
}

/// Where the plan's last segment really ends: one frame after its last
/// packet. `Err` names the plan index of the segment that could not be read.
fn last_segment_end(opened: &mut Opened, plan: &CopyPlan) -> Result<i64, (usize, String)> {
    let Some(i) = plan.segments.len().checked_sub(1) else {
        return Err((0, "plan has no segments".to_owned()));
    };
    let seg = plan.segments[i];
    seek_to(opened, seg.start).map_err(|d| (i, d))?;
    let packets = collect(opened, seg, true, &mut None).map_err(|d| (i, d))?;
    packets_end(&packets).ok_or_else(|| (i, "no packets".to_owned()))
}

/// Bytes in each NAL unit's length prefix, from the avcC record.
fn nal_length_size(avcc: &[u8]) -> usize {
    avcc.get(4).map_or(4, |b| usize::from(b & 0x03) + 1)
}

/// Whether an AVCC access unit holds an IDR slice (NAL unit type 5).
fn has_idr(data: &[u8], length_size: usize) -> bool {
    let mut rest = data;
    while rest.len() > length_size {
        let (len_bytes, tail) = rest.split_at(length_size);
        let len = len_bytes
            .iter()
            .fold(0usize, |n, &b| (n << 8) | usize::from(b));
        let Some(&header) = tail.first() else {
            return false;
        };
        if len == 0 || len > tail.len() {
            return false;
        }
        if header & 0x1f == 5 {
            return true;
        }
        rest = &tail[len..];
    }
    false
}

/// One frame past the last presented packet, the frame length taken from the
/// gap between the last two presentation times.
fn packets_end(packets: &[CopyPacket]) -> Option<i64> {
    let mut pts: Vec<i64> = packets.iter().map(|p| p.pts).collect();
    pts.sort_unstable();
    match pts[..] {
        [.., before, last] => Some(last + (last - before).max(1)),
        [only] => Some(only + 1),
        [] => None,
    }
}

/// Every video packet of `seg` in decode order: from the keyframe at its
/// start up to, not including, the keyframe at its end. `pending` carries the
/// packet that ended the previous segment when segments are read in a row.
fn collect(
    opened: &mut Opened,
    seg: CopySegment,
    last: bool,
    pending: &mut Option<CopyPacket>,
) -> Result<Vec<CopyPacket>, String> {
    let mut out: Vec<CopyPacket> = Vec::new();
    let mut skipped = 0usize;
    loop {
        let next = match pending.take() {
            Some(p) => Some(p),
            None => read_video(opened)?,
        };
        let Some(pkt) = next else {
            if out.is_empty() {
                return Err(format!("end of file before the keyframe at {}", seg.start));
            }
            if !last {
                return Err(format!("end of file before the keyframe at {}", seg.end));
            }
            return Ok(out);
        };
        if out.is_empty() {
            if pkt.keyframe && pkt.pts == seg.start {
                // Matroska marks recovery-point I pictures as keyframes too;
                // only an IDR picture drops every earlier reference.
                if !has_idr(&pkt.data, nal_length_size(&opened.track.avcc)) {
                    return Err(format!("keyframe at {} is not an IDR picture", seg.start));
                }
                out.push(pkt);
            } else if pkt.keyframe && pkt.pts > seg.start {
                return Err(format!(
                    "keyframe at {} where the index has one at {}",
                    pkt.pts, seg.start
                ));
            } else {
                skipped += 1;
                if skipped > MAX_SKIPPED_PACKETS {
                    return Err(format!(
                        "{skipped} packets after the seek without the keyframe at {}",
                        seg.start
                    ));
                }
            }
            continue;
        }
        if !last && pkt.keyframe && pkt.pts >= seg.end {
            if pkt.pts != seg.end {
                return Err(format!(
                    "keyframe at {} where the index has one at {}",
                    pkt.pts, seg.end
                ));
            }
            *pending = Some(pkt);
            return Ok(out);
        }
        out.push(pkt);
        if out.len() > MAX_SEGMENT_PACKETS {
            return Err(format!("more than {MAX_SEGMENT_PACKETS} packets"));
        }
    }
}

/// The fMP4 bytes of one segment, which ends exactly at the plan's end.
fn write(
    track: &CopyTrack,
    index: usize,
    seg: CopySegment,
    last: bool,
    packets: &[CopyPacket],
) -> Result<Vec<u8>, String> {
    if last {
        if let Some(end) = packets_end(packets).filter(|&e| e > seg.end) {
            return Err(format!(
                "packets run to {end}, past the plan's end at {}",
                seg.end
            ));
        }
    }
    let end = seg.end;
    let sequence = u32::try_from(index + 1).map_err(|_| format!("segment index {index}"))?;
    copy_fmp4::media_segment(track, sequence, seg.start, end, packets).map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn only_an_access_unit_with_an_idr_slice_starts_a_segment() {
        // SEI (6) then IDR slice (5), 4-byte lengths.
        let idr = [0, 0, 0, 2, 0x06, 0xff, 0, 0, 0, 2, 0x65, 0x88];
        assert!(has_idr(&idr, 4));
        // SEI then a non-IDR slice (1), as a recovery-point I picture is.
        let non_idr = [0, 0, 0, 2, 0x06, 0xff, 0, 0, 0, 2, 0x41, 0x9a];
        assert!(!has_idr(&non_idr, 4));
        // A length past the end is malformed, not an IDR.
        assert!(!has_idr(&[0, 0, 0, 9, 0x65], 4));
        // 2-byte lengths, as lengthSizeMinusOne = 1 declares.
        assert!(has_idr(&[0, 1, 0x65], 2));
        assert_eq!(nal_length_size(&[1, 0x64, 0, 0x1f, 0xfd]), 2);
        assert_eq!(nal_length_size(&[1, 0x64, 0, 0x1f, 0xff]), 4);
    }

    fn pkt(pts: i64, keyframe: bool) -> CopyPacket {
        CopyPacket {
            data: vec![0, 0, 0, 1, 0x65],
            pts,
            keyframe,
        }
    }

    fn track() -> CopyTrack {
        CopyTrack {
            avcc: vec![1, 0x64, 0, 0x1f, 0xff, 0xe0, 0x00],
            width: 16,
            height: 16,
            sar: None,
            timescale: 1000,
        }
    }

    #[test]
    fn packets_end_is_one_frame_past_the_last_presented_packet() {
        // Decode order differs from presentation order with B-frames.
        let packets = [pkt(0, true), pkt(80, false), pkt(40, false)];
        assert_eq!(packets_end(&packets), Some(120));
        assert_eq!(packets_end(&[pkt(7, true)]), Some(8));
        assert_eq!(packets_end(&[]), None);
    }

    #[test]
    fn last_segment_past_the_plan_end_is_refused_not_stretched() {
        let seg = CopySegment { start: 0, end: 100 };
        let packets = [pkt(0, true), pkt(40, false), pkt(80, false)];
        let err = write(&track(), 3, seg, true, &packets).expect_err("runs past the plan");
        assert!(err.contains("past the plan's end at 100"), "{err}");
    }
}
