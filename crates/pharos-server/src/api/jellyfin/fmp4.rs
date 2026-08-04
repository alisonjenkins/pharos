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
//! ## The timeline problem this module (and the encoder args) solve
//!
//! pharos generates HLS segments **per-segment on demand**: segment N is an
//! independent `ffmpeg -ss {N*6} -t 6` run. That model is trivial for MPEG-TS
//! (each `.ts` is self-contained) but fMP4 is stricter — every media segment
//! must share ONE init segment and carry a *continuous* baseMediaDecodeTime
//! (`tfdt`). ffmpeg's mp4 muxer resets `tfdt` to 0 for every independent run,
//! so naively-concatenated per-segment fMP4 collapses onto t=0 and only the
//! first segment plays (proven empirically against hls.js).
//!
//! An earlier iteration repaired this here by re-anchoring every `tfdt` onto
//! a nominal `segIndex * 6.0 s` grid. That plays — but it accumulates A/V
//! drift over a long title: a segment's real video content is ~6.012 s and
//! its audio ~6.006 s (frame-grid overhang vs opus preskip), so forcing both
//! onto the same 6.0 grid re-times the two tracks against each other by a few
//! ms EVERY segment. The fix is **source-anchoring at the encoder**
//! (`-output_ts_offset N*6 -avoid_negative_ts disabled -enc_time_base
//! 1:90000 +frag_discont`, see `pharos_transcode::ffmpeg_transcode_args`):
//! ffmpeg then writes each frame's TRUE source timestamp into `tfdt`, so
//! consecutive segments butt-join exactly and audio/video share one clock —
//! no per-segment re-anchoring, no accumulation, and segments stay
//! independently generatable (parallel encode preserved).
//!
//! [`process_segment`]'s remaining jobs:
//! 1. Split the self-contained fragmented-mp4 (`ftyp moov moof mdat …`) into an
//!    **init** (`ftyp`+`moov`) and **media** (`moof`+`mdat` pairs).
//! 2. Clamp a *negative* `tfdt` to 0: segment 0's opus preskip (312 samples)
//!    puts the first audio packet slightly below zero, which ffmpeg writes as
//!    a two's-complement negative under `-avoid_negative_ts disabled`; an
//!    unsigned reader (MSE) would see an astronomically large time and drop
//!    the fragment.
//! 3. Drop the trailing `mfra` (its `tfra` holds absolute file offsets that go
//!    stale once the init is stripped).
//!
//! The init is byte-identical across a source's segments (same encoder
//! settings ⇒ same `stsd`/`vpcC`/timescale; only cosmetic `mvhd`/`mdhd`
//! duration fields vary), so serving segment 0's init for every media segment
//! is correct — the init route just extracts and caches it.
//!
//! "Same encoder settings" is the load-bearing half of that, and it is not
//! free: the scheduler spreads ONE session's segments across every device it
//! has, and the argv differs per device. When that difference reached the
//! muxer's track timescale, segments stopped sharing their init's clock —
//! see [`record_track_timescale`], which asserts the part the sentence above
//! assumes.

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
                let ts_off = if version == 1 {
                    body + 4 + 16
                } else {
                    body + 4 + 8
                };
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

/// Clamp any *negative* `tfdt` inside one `moof` to 0, in place. ffmpeg
/// writes segment 0's opus-preskip decode time as a two's-complement
/// negative (`-avoid_negative_ts disabled`); the K-th `traf`'s track
/// timescale bounds how negative a real preskip can be (well under one
/// second), which distinguishes a wrapped negative from a legitimately
/// huge decode time on a very long title.
fn clamp_moof_tfdt(data: &mut [u8], moof: &Box, timescales: &[u32]) -> Result<(), Fmp4Error> {
    let trafs = walk(data, moof.start + moof.header..moof.end)?;
    for (traf_k, traf) in trafs.iter().filter(|b| &b.kind == b"traf").enumerate() {
        let children = walk(data, traf.start + traf.header..traf.end)?;
        for tfdt in children.iter().filter(|b| &b.kind == b"tfdt") {
            let body = tfdt.start + tfdt.header;
            let version = data.get(body).copied().unwrap_or(0);
            let ts = timescales.get(traf_k).copied().unwrap_or(48_000) as u64;
            if version == 1 {
                if body + 12 <= tfdt.end {
                    let cur =
                        u64::from_be_bytes(data[body + 4..body + 12].try_into().unwrap_or([0; 8]));
                    if (cur as i64) < 0 {
                        data[body + 4..body + 12].copy_from_slice(&0u64.to_be_bytes());
                    }
                }
            } else if body + 8 <= tfdt.end {
                let cur = u32::from_be_bytes(data[body + 4..body + 8].try_into().unwrap_or([0; 4]));
                // v0 is 32-bit: a wrapped negative lands within one second
                // (one timescale) of u32::MAX — anything that close to the
                // wrap can't be a real decode time (≈24 h @ 48 kHz).
                if (cur as u64) > u64::from(u32::MAX) - ts {
                    data[body + 4..body + 8].copy_from_slice(&0u32.to_be_bytes());
                }
            }
        }
    }
    Ok(())
}

/// Per-track timing of one segment, in seconds: the `tfdt` decode-time anchor
/// and the summed sample duration (content length). Used only for A/V-sync
/// DIAGNOSTICS — comparing consecutive segments' `tfdt + duration` (this
/// segment's end) against the next segment's `tfdt` reveals a boundary gap or
/// overlap (the mechanism behind audible clicks + drift), and comparing the
/// audio track's duration against the video track's reveals a per-segment
/// content-length mismatch that accumulates.
#[derive(Debug, Clone, Copy)]
pub struct TrackTiming {
    pub timescale: u32,
    pub tfdt_secs: f64,
    pub duration_secs: f64,
}

/// Read `(timescale, tfdt, Σ sample_duration)` per track across ALL moofs of
/// the segment (frag_keyframe emits several fragments per segment), in `traf`
/// order (index K = the K-th track's mdhd timescale). `tfdt` is the FIRST
/// fragment's (segment start); `duration_secs` sums every fragment's samples
/// (the whole segment's content length). Best-effort — a parse miss yields an
/// empty/short vec rather than an error; this is a diagnostic, never
/// load-bearing.
pub fn segment_track_timing(raw: &[u8]) -> Vec<TrackTiming> {
    let Ok(top) = walk(raw, 0..raw.len()) else {
        return Vec::new();
    };
    let Some(moov) = top.iter().find(|b| &b.kind == b"moov") else {
        return Vec::new();
    };
    let timescales = track_timescales(raw, moov).unwrap_or_default();
    // Accumulate per traf index: (timescale, first tfdt, summed duration).
    let mut acc: Vec<(u32, Option<u64>, u64)> = Vec::new();
    for moof in top.iter().filter(|b| &b.kind == b"moof") {
        let Ok(trafs) = walk(raw, moof.start + moof.header..moof.end) else {
            continue;
        };
        for (k, traf) in trafs.iter().filter(|b| &b.kind == b"traf").enumerate() {
            let ts = timescales.get(k).copied().unwrap_or(48_000);
            let Ok(children) = walk(raw, traf.start + traf.header..traf.end) else {
                continue;
            };
            let tfdt = read_tfdt(raw, &children);
            let dur = read_trun_duration(raw, &children);
            if k == acc.len() {
                acc.push((ts, tfdt, dur));
            } else if let Some(e) = acc.get_mut(k) {
                e.1 = e.1.or(tfdt); // keep the first fragment's tfdt
                e.2 += dur;
            }
        }
    }
    acc.into_iter()
        .map(|(ts, tfdt, dur)| {
            let ts_f = ts.max(1) as f64;
            TrackTiming {
                timescale: ts,
                tfdt_secs: tfdt.unwrap_or(0) as f64 / ts_f,
                duration_secs: dur as f64 / ts_f,
            }
        })
        .collect()
}

/// Whether a produced segment sits on the one timescale its rendition's init
/// declares. Carries the offending value so the log names it rather than a
/// bare class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimescaleVerdict {
    /// The segment is on [`FMP4_TRACK_TIMESCALE`] — it tiles under the init.
    Pinned,
    /// The segment is on a DIFFERENT timescale. The player divides its `tfdt`
    /// and sample durations by the init's timescale regardless, so the whole
    /// segment lands at `actual / expected` times its true position.
    Adrift { actual: u32 },
    /// No `moov`/`traf` to read a timescale from — nothing asserted.
    Unknown,
}

impl TimescaleVerdict {
    /// Bounded metric label — three stable strings, never the raw timescale
    /// (that belongs in the log line, not in a label's cardinality).
    pub fn label(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Adrift { .. } => "adrift",
            Self::Unknown => "unknown",
        }
    }
}

/// Judge one produced segment's video-track timescale against the pinned one.
pub fn check_track_timescale(raw: &[u8], expected: u32) -> TimescaleVerdict {
    // traf index 0 is the video track: the fMP4 rungs are video-only, and the
    // muxed VP9 rung maps video first.
    match segment_track_timing(raw).first().map(|t| t.timescale) {
        None => TimescaleVerdict::Unknown,
        Some(ts) if ts == expected => TimescaleVerdict::Pinned,
        Some(actual) => TimescaleVerdict::Adrift { actual },
    }
}

/// Record that judgement. A segment muxed on a timescale its init does not
/// declare plays back time-stretched by the ratio of the two — the client sees
/// a fragment far outside the range the playlist promised, refuses to join it
/// to the buffer, and refetches it forever (black screen, perpetual buffering).
/// Nothing on the success path revealed this: the encoder produced exactly the
/// frames asked for, so the frame-completeness check passed while the clock the
/// frames were stamped on was wrong.
pub fn record_track_timescale(raw: &[u8], media_id: u64, seg: u32) {
    let verdict = check_track_timescale(raw, pharos_transcode::FMP4_TRACK_TIMESCALE);
    metrics::counter!("pharos_segment_timescale_total", "verdict" => verdict.label()).increment(1);
    if let TimescaleVerdict::Adrift { actual } = verdict {
        tracing::warn!(
            media.id = media_id,
            seg,
            timescale = actual,
            expected = pharos_transcode::FMP4_TRACK_TIMESCALE,
            stretch = pharos_transcode::FMP4_TRACK_TIMESCALE as f64 / actual.max(1) as f64,
            "fmp4 segment muxed on a timescale its init does not declare — the \
             client reads it on the init's clock and lands it off its true position"
        );
    }
}

/// The movie timescale from `moov/mvhd` — the unit an `elst`'s
/// `segment_duration` is expressed in (its `media_time`, confusingly, is on the
/// track's own media timescale; only the duration is needed here).
fn movie_timescale(data: &[u8], moov: &Box) -> Option<u32> {
    let mvhd = walk(data, moov.start + moov.header..moov.end)
        .ok()?
        .into_iter()
        .find(|b| &b.kind == b"mvhd")?;
    let body = mvhd.start + mvhd.header;
    let version = data.get(body).copied().unwrap_or(0);
    // mvhd: version(1) flags(3) then 32-bit (v0) or 64-bit (v1)
    // creation/modification times before the timescale.
    let ts_off = if version == 1 {
        body + 4 + 16
    } else {
        body + 4 + 8
    };
    data.get(ts_off..ts_off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_be_bytes)
        .filter(|ts| *ts > 0)
}

/// Byte length of one `elst` entry at the given box version.
const fn elst_entry_len(version: u8) -> usize {
    // v0: u32 segment_duration, i32 media_time, u32 media_rate.
    // v1: u64 segment_duration, i64 media_time, u32 media_rate.
    if version == 1 {
        20
    } else {
        12
    }
}

/// One `elst` entry that DEFERS the track: an empty edit (`media_time == -1`)
/// of `ticks` movie-timescale ticks.
struct EmptyEdit {
    /// Empty-edit length, in movie-timescale ticks.
    ticks: u64,
}

/// Every empty edit in an init's `moov`, innermost detail included.
///
/// Returns nothing (rather than erroring) for anything it cannot rewrite
/// safely — a 64-bit `largesize` ancestor, a truncated box — because every
/// caller's fallback is "serve the bytes unchanged", which is what the code did
/// before this existed.
fn find_empty_edits(data: &[u8]) -> Vec<EmptyEdit> {
    let mut out = Vec::new();
    let Ok(top) = walk(data, 0..data.len()) else {
        return out;
    };
    let Some(moov) = top.iter().find(|b| &b.kind == b"moov") else {
        return out;
    };
    let Ok(moov_children) = walk(data, moov.start + moov.header..moov.end) else {
        return out;
    };
    for trak in moov_children.iter().filter(|b| &b.kind == b"trak") {
        let Ok(trak_children) = walk(data, trak.start + trak.header..trak.end) else {
            continue;
        };
        for edts in trak_children.iter().filter(|b| &b.kind == b"edts") {
            let Ok(edts_children) = walk(data, edts.start + edts.header..edts.end) else {
                continue;
            };
            for elst in edts_children.iter().filter(|b| &b.kind == b"elst") {
                // Only 32-bit box headers can be rewritten by decrementing a
                // u32 size in place; a largesize ancestor is left alone.
                if [moov, trak, edts, elst].iter().any(|b| b.header != 8) {
                    continue;
                }
                let body = elst.start + elst.header;
                let version = data.get(body).copied().unwrap_or(0);
                let Some(count) = data
                    .get(body + 4..body + 8)
                    .and_then(|s| s.try_into().ok())
                    .map(u32::from_be_bytes)
                else {
                    continue;
                };
                let per = elst_entry_len(version);
                for i in 0..count as usize {
                    let start = body + 8 + i * per;
                    let end = start + per;
                    if end > elst.end {
                        break;
                    }
                    // media_time follows segment_duration and is signed; -1
                    // marks an EMPTY edit — dead time before the track's media
                    // starts, i.e. a presentation offset.
                    let (ticks, media_time) = if version == 1 {
                        let d =
                            u64::from_be_bytes(data[start..start + 8].try_into().unwrap_or([0; 8]));
                        let m = i64::from_be_bytes(
                            data[start + 8..start + 16].try_into().unwrap_or([0; 8]),
                        );
                        (d, m)
                    } else {
                        let d =
                            u32::from_be_bytes(data[start..start + 4].try_into().unwrap_or([0; 4]));
                        let m = i32::from_be_bytes(
                            data[start + 4..start + 8].try_into().unwrap_or([0; 4]),
                        );
                        (u64::from(d), i64::from(m))
                    };
                    if media_time == -1 && ticks > 0 {
                        out.push(EmptyEdit { ticks });
                    }
                }
            }
        }
    }
    out
}

/// How long an init segment defers its track's presentation by, in seconds —
/// the total of every EMPTY edit in its edit list, or `None` when it defers
/// nothing.
///
/// ffmpeg writes such an edit whenever a session was produced with
/// `-output_ts_offset`, which is exactly what the demuxed audio rendition's
/// SEEK sessions use. That makes the init a property of the session that wrote
/// it rather than of the codec, which is the assumption the rest of the
/// rendition is built on (see `HlsSegmentCache::resolve_audio_file`).
pub fn init_empty_edit_secs(init: &[u8]) -> Option<f64> {
    let top = walk(init, 0..init.len()).ok()?;
    let moov = top.iter().find(|b| &b.kind == b"moov")?;
    let timescale = movie_timescale(init, moov)?;
    let ticks: u64 = find_empty_edits(init).iter().map(|e| e.ticks).sum();
    (ticks > 0).then(|| ticks as f64 / f64::from(timescale))
}

/// Record what the audio rendition's shared init declares about its own
/// timeline, for every init served.
///
/// This is the signal the browser-side failure had none of. Every fragment of a
/// seek session is re-anchored onto the absolute timeline by [`shift_tfdt`], so
/// an init that ALSO defers the track applies the offset twice: the audio lands
/// at double its true position, never overlaps the video's buffered range, and
/// the player loads fragment after fragment without ever reaching a playable
/// state. Nothing in the request path said so — every segment was produced
/// `ok`, every response was a 200 — and the only visible trace was a client
/// that fetched a whole title's worth of segments in seven minutes and never
/// posted a playback start.
///
/// LogQL: `{namespace="pharos"} |= "audio rendition init defers"`
/// PromQL: `sum by (shape) (pharos_audio_init_total)`
pub fn record_audio_init_shape(init: &[u8], media_id: u64, session_start_seg: u32) -> Option<f64> {
    let deferred = init_empty_edit_secs(init);
    let shape = if deferred.is_some() {
        "deferred"
    } else {
        "neutral"
    };
    metrics::counter!("pharos_audio_init_total", "shape" => shape).increment(1);
    if let Some(secs) = deferred {
        tracing::warn!(
            media.id = media_id,
            session_start_seg,
            empty_edit_secs = secs,
            "audio rendition init defers its track — the fragments served under it \
             are re-anchored to the same timeline position, so a player honouring \
             this edit places the audio at twice its true offset"
        );
    }
    deferred
}

/// The `tfdt` base decode time (0-clamped) from a traf's children, if present.
fn read_tfdt(raw: &[u8], children: &[Box]) -> Option<u64> {
    let b = children.iter().find(|b| &b.kind == b"tfdt")?;
    let body = b.start + b.header;
    let ver = raw.get(body).copied().unwrap_or(0);
    let v = if ver == 1 {
        raw.get(body + 4..body + 12)
            .and_then(|s| s.try_into().ok())
            .map(u64::from_be_bytes)?
    } else {
        raw.get(body + 4..body + 8)
            .and_then(|s| s.try_into().ok())
            .map(u32::from_be_bytes)
            .map(u64::from)?
    };
    Some(if (v as i64) < 0 { 0 } else { v })
}

/// Sum a traf's sample durations from its `trun` (falling back to the `tfhd`
/// `default_sample_duration` when the trun omits per-sample durations).
fn read_trun_duration(raw: &[u8], children: &[Box]) -> u64 {
    let read_flags = |body: usize| {
        u32::from_be_bytes([
            0,
            raw.get(body + 1).copied().unwrap_or(0),
            raw.get(body + 2).copied().unwrap_or(0),
            raw.get(body + 3).copied().unwrap_or(0),
        ])
    };
    let mut default_dur: u32 = 0;
    if let Some(b) = children.iter().find(|b| &b.kind == b"tfhd") {
        let body = b.start + b.header;
        let flags = read_flags(body);
        let mut p = body + 8; // fullbox(4) + track_ID(4)
        if flags & 0x01 != 0 {
            p += 8;
        }
        if flags & 0x02 != 0 {
            p += 4;
        }
        if flags & 0x08 != 0 {
            default_dur = raw
                .get(p..p + 4)
                .and_then(|s| s.try_into().ok())
                .map(u32::from_be_bytes)
                .unwrap_or(0);
        }
    }
    let Some(b) = children.iter().find(|b| &b.kind == b"trun") else {
        return 0;
    };
    let body = b.start + b.header;
    let flags = read_flags(body);
    let count = raw
        .get(body + 4..body + 8)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_be_bytes)
        .unwrap_or(0);
    if flags & 0x000100 == 0 {
        return u64::from(count) * u64::from(default_dur);
    }
    let mut p = body + 8;
    if flags & 0x000001 != 0 {
        p += 4;
    }
    if flags & 0x000004 != 0 {
        p += 4;
    }
    let per = 4 // duration present (0x100)
        + (flags & 0x000200 != 0) as usize * 4
        + (flags & 0x000400 != 0) as usize * 4
        + (flags & 0x000800 != 0) as usize * 4;
    let mut sum = 0u64;
    for _ in 0..count {
        if let Some(d) = raw
            .get(p..p + 4)
            .and_then(|s| s.try_into().ok())
            .map(u32::from_be_bytes)
        {
            sum += u64::from(d);
        }
        p += per;
    }
    sum
}

/// The media timescale of an init segment's first track, if it has one.
pub fn init_timescale(init: &[u8]) -> Option<u32> {
    let top = walk(init, 0..init.len()).ok()?;
    let moov = top.iter().find(|b| &b.kind == b"moov")?;
    track_timescales(init, moov)
        .ok()?
        .into_iter()
        .find(|ts| *ts > 0)
}

/// Move every `tfdt` in `data` by `delta` ticks, in place.
///
/// The demuxed audio rendition is produced by ffmpeg's HLS muxer, which numbers
/// each session's fragments from that session's own first sample — a fragment
/// written by a session that began mid-title carries its offset WITHIN the
/// session, so a player that trusts it puts the audio minutes early (B121).
/// The fragments themselves are correct; only their anchor needs moving, and
/// shifting rather than overwriting keeps any internal multi-fragment spacing.
///
/// A `tfdt` too small to absorb a negative `delta` is clamped to 0 rather than
/// wrapped: an unsigned reader treats a wrapped value as an astronomical time
/// and drops the fragment.
pub fn shift_tfdt(data: &mut [u8], delta: i64) -> Result<(), Fmp4Error> {
    if delta == 0 {
        return Ok(());
    }
    let top = walk(data, 0..data.len())?;
    let moofs: Vec<(usize, usize, usize)> = top
        .iter()
        .filter(|b| &b.kind == b"moof")
        .map(|b| (b.start, b.header, b.end))
        .collect();
    for (start, header, end) in moofs {
        let trafs: Vec<(usize, usize, usize)> = walk(data, start + header..end)?
            .iter()
            .filter(|b| &b.kind == b"traf")
            .map(|b| (b.start, b.header, b.end))
            .collect();
        for (t_start, t_header, t_end) in trafs {
            let tfdts: Vec<(usize, usize, usize)> = walk(data, t_start + t_header..t_end)?
                .iter()
                .filter(|b| &b.kind == b"tfdt")
                .map(|b| (b.start, b.header, b.end))
                .collect();
            for (b_start, b_header, b_end) in tfdts {
                let body = b_start + b_header;
                let version = data.get(body).copied().unwrap_or(0);
                if version == 1 {
                    if body + 12 <= b_end {
                        let cur = u64::from_be_bytes(
                            data[body + 4..body + 12].try_into().unwrap_or([0; 8]),
                        );
                        let next = cur.saturating_add_signed(delta);
                        data[body + 4..body + 12].copy_from_slice(&next.to_be_bytes());
                    }
                } else if body + 8 <= b_end {
                    let cur =
                        u32::from_be_bytes(data[body + 4..body + 8].try_into().unwrap_or([0; 4]));
                    // v0 is 32-bit; a shifted value past its range would wrap,
                    // so saturate — the caller's own range check is what keeps
                    // this from mattering in practice.
                    let next = u64::from(cur)
                        .saturating_add_signed(delta)
                        .min(u64::from(u32::MAX)) as u32;
                    data[body + 4..body + 8].copy_from_slice(&next.to_be_bytes());
                }
            }
        }
    }
    Ok(())
}

/// Split a self-contained fragmented-mp4 segment into a shared init and a
/// media segment whose (already source-anchored) `tfdt`s are passed through
/// verbatim, with negatives clamped to 0 (see the module docs).
pub fn process_segment(raw: &[u8]) -> Result<Processed, Fmp4Error> {
    let top = walk(raw, 0..raw.len())?;
    let moov = top
        .iter()
        .find(|b| &b.kind == b"moov")
        .ok_or(Fmp4Error::NoMoov)?;
    if !top.iter().any(|b| &b.kind == b"moof") {
        return Err(Fmp4Error::NoMoof);
    }

    // Init = ftyp (if present) + moov, verbatim.
    let mut init = Vec::new();
    for b in top
        .iter()
        .filter(|b| &b.kind == b"ftyp" || &b.kind == b"moov")
    {
        init.extend_from_slice(&raw[b.start..b.end]);
    }

    let timescales = track_timescales(raw, moov)?;

    // Media = every moof+mdat (and any other post-moov boxes) EXCEPT the
    // trailing mfra, whose tfra offsets are invalidated by stripping the init.
    // Copy into a mutable buffer and clamp each moof's tfdt in place.
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
            if size32 == 1 {
                16
            } else {
                8
            }
        };
        let moof = Box {
            kind: *b"moof",
            start: span.start,
            end: span.end,
            header,
        };
        clamp_moof_tfdt(&mut media, &moof, &timescales)?;
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

    /// A segment carrying ONE video track on the given timescale.
    fn segment_on_timescale(timescale: u32) -> Vec<u8> {
        let ftyp = mk_box(b"ftyp", b"isom");
        let moov = mk_box(b"moov", &trak(timescale));
        let moof = mk_box(b"moof", &traf(0));
        let mdat = mk_box(b"mdat", &[0xAA; 32]);
        let mut seg = Vec::new();
        for part in [&ftyp, &moov, &moof, &mdat] {
            seg.extend_from_slice(part);
        }
        seg
    }

    #[test]
    fn a_segment_on_the_pinned_timescale_is_accepted() {
        let seg = segment_on_timescale(pharos_transcode::FMP4_TRACK_TIMESCALE);
        assert_eq!(
            check_track_timescale(&seg, pharos_transcode::FMP4_TRACK_TIMESCALE),
            TimescaleVerdict::Pinned
        );
    }

    #[test]
    fn a_segment_on_another_timescale_is_flagged_with_the_value() {
        // The live shape: the init declared 24000 (a hardware encode pinned by
        // `-r 24000/1001`) while this segment came off the software encoder's
        // 90000 grid, so the client read it 3.75x out.
        let seg = segment_on_timescale(24_000);
        assert_eq!(
            check_track_timescale(&seg, 90_000),
            TimescaleVerdict::Adrift { actual: 24_000 },
            "the verdict must carry the offending timescale, not a bare class"
        );
    }

    #[test]
    fn a_segment_with_no_moov_asserts_nothing() {
        let seg = mk_box(b"mdat", &[0xAA; 8]);
        assert_eq!(
            check_track_timescale(&seg, 90_000),
            TimescaleVerdict::Unknown
        );
    }

    #[test]
    fn timescale_verdict_labels_are_distinct_and_bounded() {
        let labels = [
            TimescaleVerdict::Pinned.label(),
            TimescaleVerdict::Adrift { actual: 24_000 }.label(),
            TimescaleVerdict::Unknown.label(),
        ];
        let distinct: std::collections::BTreeSet<_> = labels.iter().collect();
        assert_eq!(distinct.len(), labels.len(), "labels collide: {labels:?}");
        // The raw timescale must never reach a label — that is unbounded
        // cardinality on a dashboard contract.
        assert!(!labels.iter().any(|l| l.contains("24000")));
    }

    #[test]
    fn splits_init_and_media() {
        let seg = sample_segment();
        let p = process_segment(&seg).unwrap();
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
    fn preserves_source_anchored_tfdt() {
        // ffmpeg now writes the TRUE source timestamps (`-output_ts_offset`
        // + `-avoid_negative_ts disabled` — see pharos-transcode). The
        // surgery must pass them through verbatim: re-anchoring onto a
        // nominal 6.0 s grid is exactly the bug that accumulated A/V drift.
        let ftyp = mk_box(b"ftyp", b"isom");
        let mut moov_body = Vec::new();
        moov_body.extend_from_slice(&trak(15360));
        moov_body.extend_from_slice(&trak(48000));
        let moov = mk_box(b"moov", &moov_body);
        let mut moof_body = Vec::new();
        // Segment 3 at source-anchored times: video 18.006 s, audio 17.9935 s.
        moof_body.extend_from_slice(&traf(276_572)); // 18.006 * 15360
        moof_body.extend_from_slice(&traf(863_688)); // 17.9935 * 48000
        let moof = mk_box(b"moof", &moof_body);
        let mdat = mk_box(b"mdat", &[0xAA; 32]);
        let mut seg = Vec::new();
        for part in [&ftyp, &moov, &moof, &mdat] {
            seg.extend_from_slice(part);
        }
        let p = process_segment(&seg).unwrap();
        assert_eq!(read_tfdt(&p.media), vec![276_572, 863_688]);
    }

    #[test]
    fn clamps_negative_tfdt_to_zero() {
        // Segment 0's opus preskip (312 samples) puts the first audio
        // packet's decode time slightly BELOW zero; with
        // `-avoid_negative_ts disabled` ffmpeg writes it as a two's-
        // complement negative, which an unsigned tfdt reader (MSE) sees as
        // an astronomically large time and drops the fragment. Clamp to 0.
        let ftyp = mk_box(b"ftyp", b"isom");
        let moov = mk_box(b"moov", &trak(48000));
        let moof = mk_box(b"moof", &traf((-312i64) as u64));
        let mdat = mk_box(b"mdat", &[0; 8]);
        let mut seg = Vec::new();
        for part in [&ftyp, &moov, &moof, &mdat] {
            seg.extend_from_slice(part);
        }
        let p = process_segment(&seg).unwrap();
        assert_eq!(read_tfdt(&p.media), vec![0]);
    }

    #[test]
    fn segment_zero_leaves_tfdt_untouched() {
        let seg = sample_segment();
        let p = process_segment(&seg).unwrap();
        assert_eq!(read_tfdt(&p.media), vec![0, 0]);
    }

    /// B121 — a seek session's fragments carry their offset WITHIN that
    /// session; shifting by the session's start puts them back on the timeline,
    /// and every track in the fragment moves together (they share one clock).
    #[test]
    fn shifting_moves_every_tfdt_by_the_same_delta() {
        let mut media = Vec::new();
        media.extend_from_slice(&mk_box(b"moof", &[traf(0), traf(0)].concat()));
        media.extend_from_slice(&mk_box(b"mdat", b"xx"));
        media.extend_from_slice(&mk_box(b"moof", &[traf(288_000), traf(288_000)].concat()));
        media.extend_from_slice(&mk_box(b"mdat", b"yy"));

        shift_tfdt(&mut media, 225 * 6 * 48_000).unwrap();

        let want = 225 * 6 * 48_000;
        assert_eq!(
            read_tfdt(&media),
            vec![want, want, want + 288_000, want + 288_000]
        );
    }

    /// A fragment already on the timeline (the whole-file session's) must come
    /// through untouched — the shift is only ever the session's own start.
    #[test]
    fn shifting_by_zero_changes_nothing() {
        let mut media = mk_box(b"moof", &traf(4_896_000));
        let before = media.clone();
        shift_tfdt(&mut media, 0).unwrap();
        assert_eq!(media, before);
    }

    /// A shift that would take a `tfdt` below zero saturates instead of
    /// wrapping: an unsigned reader treats a wrapped value as an astronomical
    /// decode time and drops the fragment.
    #[test]
    fn shifting_below_zero_saturates() {
        let mut media = mk_box(b"moof", &traf(100));
        shift_tfdt(&mut media, -500).unwrap();
        assert_eq!(read_tfdt(&media), vec![0]);
    }

    #[test]
    fn rejects_non_fragmented_input() {
        let ftyp = mk_box(b"ftyp", b"isom");
        let moov = mk_box(b"moov", &trak(15360));
        let mut seg = Vec::new();
        seg.extend_from_slice(&ftyp);
        seg.extend_from_slice(&moov);
        assert!(matches!(process_segment(&seg), Err(Fmp4Error::NoMoof)));
    }
}
