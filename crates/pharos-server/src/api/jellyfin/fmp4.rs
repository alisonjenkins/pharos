//! fMP4 (fragmented-MP4) segment surgery for the VP9-in-HLS path.
//!
//! ## Why this exists
//!
//! Firefox/Zen cannot decode H.264 in MSE (no bundled licensed decoder on
//! codec-less Linux), so pharos transcodes to VP9 for those clients. A
//! progressive `<video src>` WebM stream plays but cannot seek to an
//! unbuffered position and does not report a reliable playback position for
//! resume. The fix — matching how Jellyfin serves browser transcodes — is
//! **VP9-in-fMP4 HLS**: hls.js gets a VOD playlist of `.m4s` segments plus a
//! shared init segment, which gives seeking, resume, and track-switching.
//!
//! ## The tfdt problem this module solves
//!
//! pharos generates HLS segments **per-segment on demand**: segment N is an
//! independent `ffmpeg -ss {N*6} -t 6` run. That model is trivial for MPEG-TS
//! (each `.ts` is self-contained) but fMP4 is stricter — every media segment
//! must share ONE init segment and carry a *continuous* baseMediaDecodeTime
//! (`tfdt`). ffmpeg's mp4 muxer resets `tfdt` to 0 for every independent run,
//! so naively-concatenated per-segment fMP4 collapses onto t=0 and only the
//! first segment plays (proven empirically against hls.js).
//!
//! [`process_segment`] repairs this after ffmpeg:
//! 1. Split the self-contained fragmented-mp4 (`ftyp moov moof mdat …`) into an
//!    **init** (`ftyp`+`moov`) and **media** (`moof`+`mdat` pairs).
//! 2. Rewrite each fragment's `tfdt` to `original + segIndex*segSeconds*track_timescale`,
//!    per track, so the segment lands at its true position on the global
//!    timeline. Adding (not overwriting) preserves any intra-segment fragment
//!    offsets when a segment carries more than one `moof`.
//! 3. Drop the trailing `mfra` (its `tfra` holds absolute file offsets that go
//!    stale once the init is stripped).
//!
//! The init is byte-identical across a source's segments (same encoder
//! settings ⇒ same `stsd`/`vpcC`/timescale; only cosmetic `mvhd`/`mdhd`
//! duration fields vary), so serving segment 0's init for every media segment
//! is correct — the init route just extracts and caches it.

/// Nominal HLS segment length in seconds. Must match the value the variant
/// playlist advertises and the `-ss/-t` window the segment generator uses, so
/// the computed `tfdt` base (`seg_index * SEGMENT_SECONDS * timescale`) lines
/// up with where the segment actually sits on the timeline.
pub const SEGMENT_SECONDS: f64 = 6.0;

#[derive(Debug, thiserror::Error)]
pub enum Fmp4Error {
    #[error("fmp4: truncated box at offset {0}")]
    Truncated(usize),
    #[error("fmp4: no moov box (not a fragmented-mp4 segment?)")]
    NoMoov,
    #[error("fmp4: no moof box (empty media segment?)")]
    NoMoof,
}

/// A processed fMP4 segment: the shared init (`ftyp`+`moov`) and the
/// timeline-corrected media (`moof`+`mdat` fragments).
pub struct Processed {
    pub init: Vec<u8>,
    pub media: Vec<u8>,
}

/// One top-level ISO-BMFF box: 4CC type and its byte range in the buffer.
struct Box {
    kind: [u8; 4],
    /// Offset of the box header start.
    start: usize,
    /// Offset one past the box end.
    end: usize,
    /// Header length (8, or 16 for 64-bit largesize).
    header: usize,
}

/// Walk the direct children of `data[range]`, returning each box's span.
/// Stops (rather than erroring) on a zero/oversized size to stay robust
/// against a truncated tail — the caller validates the boxes it needs.
fn walk(data: &[u8], range: std::ops::Range<usize>) -> Result<Vec<Box>, Fmp4Error> {
    let mut out = Vec::new();
    let mut off = range.start;
    while off + 8 <= range.end {
        let size32 = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&data[off + 4..off + 8]);
        let (size, header) = if size32 == 1 {
            // 64-bit largesize follows the 4CC.
            if off + 16 > range.end {
                return Err(Fmp4Error::Truncated(off));
            }
            let s = u64::from_be_bytes(data[off + 8..off + 16].try_into().unwrap_or([0; 8]));
            (s as usize, 16usize)
        } else if size32 == 0 {
            // Box extends to end of the enclosing range.
            (range.end - off, 8usize)
        } else {
            (size32 as usize, 8usize)
        };
        let end = off.checked_add(size).ok_or(Fmp4Error::Truncated(off))?;
        if size < header || end > range.end {
            return Err(Fmp4Error::Truncated(off));
        }
        out.push(Box {
            kind,
            start: off,
            end,
            header,
        });
        off = end;
    }
    Ok(out)
}

/// Read the per-track media timescales from a `moov` box, in track order.
/// Each `trak` contributes one `mdia/mdhd` timescale; the order matches the
/// `traf` order ffmpeg writes in each `moof`, so index K here pairs with the
/// K-th `traf`'s `tfdt`.
fn track_timescales(data: &[u8], moov: &Box) -> Result<Vec<u32>, Fmp4Error> {
    let mut out = Vec::new();
    let moov_children = walk(data, moov.start + moov.header..moov.end)?;
    for trak in moov_children.iter().filter(|b| &b.kind == b"trak") {
        let trak_children = walk(data, trak.start + trak.header..trak.end)?;
        for mdia in trak_children.iter().filter(|b| &b.kind == b"mdia") {
            let mdia_children = walk(data, mdia.start + mdia.header..mdia.end)?;
            for mdhd in mdia_children.iter().filter(|b| &b.kind == b"mdhd") {
                let body = mdhd.start + mdhd.header;
                let version = data.get(body).copied().unwrap_or(0);
                // mdhd: version(1) flags(3) then either 32-bit (v0) or 64-bit
                // (v1) creation/modification times before the timescale.
                let ts_off = if version == 1 { body + 4 + 16 } else { body + 4 + 8 };
                if ts_off + 4 <= mdhd.end {
                    out.push(u32::from_be_bytes(
                        data[ts_off..ts_off + 4].try_into().unwrap_or([0; 4]),
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// Shift every `tfdt` inside one `moof` by `base_ticks[k]` for the K-th
/// `traf`, in place. Adds to the existing value so multi-fragment segments
/// keep their intra-segment offsets.
fn shift_moof_tfdt(
    data: &mut [u8],
    moof: &Box,
    base_ticks: &[u64],
) -> Result<(), Fmp4Error> {
    let trafs = walk(data, moof.start + moof.header..moof.end)?;
    for (traf_k, traf) in trafs
        .iter()
        .filter(|b| &b.kind == b"traf")
        .enumerate()
    {
        let children = walk(data, traf.start + traf.header..traf.end)?;
        for tfdt in children.iter().filter(|b| &b.kind == b"tfdt") {
            let body = tfdt.start + tfdt.header;
            let version = data.get(body).copied().unwrap_or(0);
            let base = base_ticks.get(traf_k).copied().unwrap_or(0);
            if version == 1 {
                if body + 12 <= tfdt.end {
                    let cur = u64::from_be_bytes(data[body + 4..body + 12].try_into().unwrap_or([0; 8]));
                    data[body + 4..body + 12].copy_from_slice(&cur.saturating_add(base).to_be_bytes());
                }
            } else if body + 8 <= tfdt.end {
                let cur = u32::from_be_bytes(data[body + 4..body + 8].try_into().unwrap_or([0; 4])) as u64;
                // A 32-bit tfdt can overflow once the base decode time exceeds
                // ~2^32 ticks; clamp defensively (ffmpeg emits v1 for long
                // media, so this is the short-media fast path).
                let sum = cur.saturating_add(base).min(u32::MAX as u64) as u32;
                data[body + 4..body + 8].copy_from_slice(&sum.to_be_bytes());
            }
        }
    }
    Ok(())
}

/// Split a self-contained fragmented-mp4 segment into a shared init and a
/// timeline-corrected media segment (see the module docs).
///
/// `seg_index` is the zero-based HLS segment number; `seg_seconds` the nominal
/// segment length (normally [`SEGMENT_SECONDS`]).
pub fn process_segment(
    raw: &[u8],
    seg_index: u32,
    seg_seconds: f64,
) -> Result<Processed, Fmp4Error> {
    let top = walk(raw, 0..raw.len())?;
    let moov = top.iter().find(|b| &b.kind == b"moov").ok_or(Fmp4Error::NoMoov)?;
    if !top.iter().any(|b| &b.kind == b"moof") {
        return Err(Fmp4Error::NoMoof);
    }

    // Init = ftyp (if present) + moov, verbatim.
    let mut init = Vec::new();
    for b in top.iter().filter(|b| &b.kind == b"ftyp" || &b.kind == b"moov") {
        init.extend_from_slice(&raw[b.start..b.end]);
    }

    let timescales = track_timescales(raw, moov)?;
    let base_ticks: Vec<u64> = timescales
        .iter()
        .map(|&ts| (seg_index as f64 * seg_seconds * ts as f64).round() as u64)
        .collect();

    // Media = every moof+mdat (and any other post-moov boxes) EXCEPT the
    // trailing mfra, whose tfra offsets are invalidated by stripping the init.
    // Copy into a mutable buffer and patch each moof's tfdt in place.
    let mut media = Vec::new();
    let mut moof_spans: Vec<std::ops::Range<usize>> = Vec::new();
    for b in &top {
        if &b.kind == b"ftyp" || &b.kind == b"moov" || &b.kind == b"mfra" {
            continue;
        }
        let dst_start = media.len();
        media.extend_from_slice(&raw[b.start..b.end]);
        if &b.kind == b"moof" {
            moof_spans.push(dst_start..dst_start + (b.end - b.start));
        }
    }
    // Re-walk the copied media so the moof offsets refer to `media`, then patch.
    for span in moof_spans {
        // The copied box preserves its header; rebuild a Box view over `media`.
        let header = {
            let size32 = u32::from_be_bytes([
                media[span.start],
                media[span.start + 1],
                media[span.start + 2],
                media[span.start + 3],
            ]);
            if size32 == 1 { 16 } else { 8 }
        };
        let moof = Box {
            kind: *b"moof",
            start: span.start,
            end: span.end,
            header,
        };
        shift_moof_tfdt(&mut media, &moof, &base_ticks)?;
    }

    Ok(Processed { init, media })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Build a minimal box: 4-byte size + 4CC + body.
    fn mk_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = 8 + body.len();
        let mut v = (size as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    /// mdhd v0 with a given timescale.
    fn mdhd(timescale: u32) -> Vec<u8> {
        let mut body = vec![0u8; 4 + 8]; // version/flags + creation/mod (v0: 4+4)
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&[0u8; 8]); // duration + language/quality
        mk_box(b"mdhd", &body)
    }

    fn trak(timescale: u32) -> Vec<u8> {
        let mdia = mk_box(b"mdia", &mdhd(timescale));
        mk_box(b"trak", &mdia)
    }

    /// tfdt v1 with a base decode time.
    fn tfdt(base: u64) -> Vec<u8> {
        let mut body = vec![1u8, 0, 0, 0]; // version 1
        body.extend_from_slice(&base.to_be_bytes());
        mk_box(b"tfdt", &body)
    }

    fn traf(base: u64) -> Vec<u8> {
        mk_box(b"traf", &tfdt(base))
    }

    fn read_tfdt(data: &[u8]) -> Vec<u64> {
        // tfdt is nested inside moof→traf, so byte-scan for the 4CC rather
        // than walking top-level boxes. Every tfdt here is version 1.
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            if &data[i..i + 4] == b"tfdt" && data[i + 4] == 1 {
                out.push(u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()));
            }
            i += 1;
        }
        out
    }

    fn sample_segment() -> Vec<u8> {
        // ftyp + moov(trak[15360], trak[48000]) + moof(traf[0], traf[0]) + mdat + mfra
        let ftyp = mk_box(b"ftyp", b"isom");
        let mut moov_body = Vec::new();
        moov_body.extend_from_slice(&trak(15360));
        moov_body.extend_from_slice(&trak(48000));
        let moov = mk_box(b"moov", &moov_body);
        let mut moof_body = Vec::new();
        moof_body.extend_from_slice(&traf(0));
        moof_body.extend_from_slice(&traf(0));
        let moof = mk_box(b"moof", &moof_body);
        let mdat = mk_box(b"mdat", &[0xAA; 32]);
        let mfra = mk_box(b"mfra", &[0xBB; 16]);
        let mut seg = Vec::new();
        seg.extend_from_slice(&ftyp);
        seg.extend_from_slice(&moov);
        seg.extend_from_slice(&moof);
        seg.extend_from_slice(&mdat);
        seg.extend_from_slice(&mfra);
        seg
    }

    #[test]
    fn splits_init_and_media() {
        let seg = sample_segment();
        let p = process_segment(&seg, 0, SEGMENT_SECONDS).unwrap();
        // init carries ftyp + moov, media carries moof + mdat, mfra dropped.
        assert_eq!(&p.init[4..8], b"ftyp");
        assert!(p.init.windows(4).any(|w| w == b"moov"));
        assert!(!p.init.windows(4).any(|w| w == b"moof"));
        assert!(p.media.windows(4).any(|w| w == b"moof"));
        assert!(p.media.windows(4).any(|w| w == b"mdat"));
        assert!(
            !p.media.windows(4).any(|w| w == b"mfra"),
            "mfra must be dropped (stale offsets)"
        );
    }

    #[test]
    fn patches_tfdt_per_track_timescale() {
        let seg = sample_segment();
        // Segment index 3 → video base 3*6*15360, audio base 3*6*48000.
        let p = process_segment(&seg, 3, SEGMENT_SECONDS).unwrap();
        let tfdts = read_tfdt(&p.media);
        assert_eq!(tfdts, vec![3 * 6 * 15360, 3 * 6 * 48000]);
    }

    #[test]
    fn segment_zero_leaves_tfdt_untouched() {
        let seg = sample_segment();
        let p = process_segment(&seg, 0, SEGMENT_SECONDS).unwrap();
        assert_eq!(read_tfdt(&p.media), vec![0, 0]);
    }

    #[test]
    fn adds_to_existing_offset() {
        // A fragment already at intra-segment offset 100 must keep it: seg 2,
        // video base 2*6*15360 = 184320, plus the pre-existing 100.
        let ftyp = mk_box(b"ftyp", b"isom");
        let moov = mk_box(b"moov", &trak(15360));
        let moof = mk_box(b"moof", &traf(100));
        let mdat = mk_box(b"mdat", &[0; 8]);
        let mut seg = Vec::new();
        for part in [&ftyp, &moov, &moof, &mdat] {
            seg.extend_from_slice(part);
        }
        let p = process_segment(&seg, 2, SEGMENT_SECONDS).unwrap();
        assert_eq!(read_tfdt(&p.media), vec![2 * 6 * 15360 + 100]);
    }

    #[test]
    fn rejects_non_fragmented_input() {
        let ftyp = mk_box(b"ftyp", b"isom");
        let moov = mk_box(b"moov", &trak(15360));
        let mut seg = Vec::new();
        seg.extend_from_slice(&ftyp);
        seg.extend_from_slice(&moov);
        assert!(matches!(
            process_segment(&seg, 0, SEGMENT_SECONDS),
            Err(Fmp4Error::NoMoof)
        ));
    }
}
