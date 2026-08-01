//! Which parts of a remote source are already on disk, and what a read still
//! has to fetch.
//!
//! Split out from the cache itself because this is the half most likely to be
//! wrong and the half that can be tested without a network, a filesystem or a
//! clock. An off-by-one here does not fail loudly: it serves a byte range that
//! silently contains a hole, ffmpeg sees zeros where a frame should be, and the
//! symptom arrives much later as a corrupt segment.

/// Bytes per cache chunk.
///
/// The trade is upstream round-trips against wasted transfer. Too small and a
/// linear read costs hundreds of range requests, which is the throttling this
/// cache exists to avoid; too large and a seek to one frame drags megabytes.
/// 2 MiB is roughly one HLS segment's worth of a high-bitrate 1080p source, so
/// sequential playback usually needs one fetch per segment.
pub const CHUNK: u64 = 2 * 1024 * 1024;

/// The half-open chunk index range covering a byte range.
///
/// `end` is exclusive in bytes AND in chunks. A zero-length request yields an
/// empty range rather than one chunk, so a probe that asks for nothing fetches
/// nothing.
pub fn chunks_for(start: u64, end: u64) -> std::ops::Range<u64> {
    if end <= start {
        return 0..0;
    }
    (start / CHUNK)..(end.div_ceil(CHUNK))
}

/// The byte range one chunk covers, clamped to the source's total size.
///
/// Clamping matters: the final chunk is almost always short, and asking
/// upstream for bytes past the end earns a 416 rather than a truncated body.
pub fn chunk_bytes(index: u64, total: u64) -> std::ops::Range<u64> {
    let start = index * CHUNK;
    let end = ((index + 1) * CHUNK).min(total);
    start..end.max(start)
}

/// Group the missing chunks in `wanted` into CONTIGUOUS runs.
///
/// One upstream request per run rather than per chunk. A seek into the middle
/// of a file typically wants a handful of adjacent chunks, and issuing them
/// separately is exactly the request storm the cache is meant to prevent.
pub fn missing_runs(
    wanted: std::ops::Range<u64>,
    present: &std::collections::HashSet<u64>,
) -> Vec<std::ops::Range<u64>> {
    let mut runs: Vec<std::ops::Range<u64>> = Vec::new();
    for i in wanted {
        if present.contains(&i) {
            continue;
        }
        match runs.last_mut() {
            Some(r) if r.end == i => r.end = i + 1,
            _ => runs.push(i..i + 1),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_byte_range_maps_onto_the_chunks_that_actually_cover_it() {
        // Wholly inside chunk 0.
        assert_eq!(chunks_for(0, 100), 0..1);
        // Ends exactly on a boundary — must NOT pull in the next chunk, which
        // would fetch 2 MiB nobody asked for on every aligned read.
        assert_eq!(chunks_for(0, CHUNK), 0..1);
        // One byte past it does.
        assert_eq!(chunks_for(0, CHUNK + 1), 0..2);
        // Starting exactly on a boundary starts at that chunk, not the one
        // before.
        assert_eq!(chunks_for(CHUNK, CHUNK + 1), 1..2);
        // Spanning three.
        assert_eq!(chunks_for(CHUNK - 1, 2 * CHUNK + 1), 0..3);
        // Empty and inverted ranges fetch nothing rather than one chunk.
        assert_eq!(chunks_for(500, 500), 0..0);
        assert_eq!(chunks_for(500, 100), 0..0);
    }

    #[test]
    fn the_last_chunk_is_clamped_to_the_end_of_the_source() {
        let total = CHUNK + 10;
        assert_eq!(chunk_bytes(0, total), 0..CHUNK);
        // Short tail — asking upstream for the full chunk would be a range
        // past the end, which earns a 416 rather than a short body.
        assert_eq!(chunk_bytes(1, total), CHUNK..(CHUNK + 10));
        // Entirely past the end is empty, never inverted.
        let r = chunk_bytes(9, total);
        assert!(
            r.is_empty(),
            "a chunk past the end must be empty, got {r:?}"
        );
    }

    #[test]
    fn missing_chunks_are_coalesced_into_one_request_per_run() {
        let present: HashSet<u64> = [1, 2, 6].into_iter().collect();
        // 0 | 1,2 held | 3,4,5 | 6 held | 7
        assert_eq!(missing_runs(0..8, &present), vec![0..1, 3..6, 7..8]);
        // Everything present → no upstream request at all, which is the whole
        // point of the cache on a re-read.
        let all: HashSet<u64> = (0..4).collect();
        assert!(missing_runs(0..4, &all).is_empty());
        // Nothing present → exactly one run, not four requests.
        assert_eq!(missing_runs(0..4, &HashSet::new()), vec![0..4]);
    }
}
