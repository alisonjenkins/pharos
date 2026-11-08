//! Segment timeline queries over a parsed [`MediaPlaylist`].

use std::cmp::Ordering;

use crate::playlist::{MediaPlaylist, Segment};

/// Total playlist duration in milliseconds (the end of the last segment, or 0 if empty).
pub fn duration_ms(p: &MediaPlaylist) -> u64 {
    segments_duration_ms(&p.segments)
}

/// Index of the segment whose `[start_ms, start_ms + duration_ms)` interval holds `at_ms`.
pub fn segment_at(p: &MediaPlaylist, at_ms: u64) -> Option<usize> {
    segment_at_slice(&p.segments, at_ms)
}

/// Returns `(start_ms, end_ms)` for the segment at `index`.
pub fn range_of(p: &MediaPlaylist, index: usize) -> Option<(u64, u64)> {
    range_of_slice(&p.segments, index)
}

/// Whether two renditions cut the media at the same instants: the same number
/// of segments, each starting and lasting the same. Only then can a quality
/// switch carry on at the next index without a gap or an overlap.
pub fn same_grid(a: &[Segment], b: &[Segment]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.start_ms == y.start_ms && x.duration_ms == y.duration_ms)
}

pub(crate) fn segments_duration_ms(segments: &[Segment]) -> u64 {
    segments
        .last()
        .map_or(0, |seg| seg.start_ms.saturating_add(seg.duration_ms))
}

pub(crate) fn segment_at_slice(segments: &[Segment], at_ms: u64) -> Option<usize> {
    segments
        .binary_search_by(|seg| {
            let end_ms = seg.start_ms.saturating_add(seg.duration_ms);
            if at_ms < seg.start_ms {
                Ordering::Greater
            } else if at_ms >= end_ms {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .ok()
}

pub(crate) fn range_of_slice(segments: &[Segment], index: usize) -> Option<(u64, u64)> {
    let seg = segments.get(index)?;
    Some((seg.start_ms, seg.start_ms.saturating_add(seg.duration_ms)))
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected results is the intended failure mode in tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use proptest::prelude::*;

    use super::{duration_ms, range_of, same_grid, segment_at};
    use crate::playlist::{parse_media, Segment};

    #[test]
    fn same_grid_compares_timing_not_uris() {
        let seg = |uri: &str, start_ms, duration_ms| Segment {
            uri: uri.to_owned(),
            start_ms,
            duration_ms,
        };
        let a = [seg("a/0.m4s", 0, 6_000), seg("a/1.m4s", 6_000, 4_000)];
        let b = [seg("b/0.m4s", 0, 6_000), seg("b/1.m4s", 6_000, 4_000)];
        assert!(same_grid(&a, &b));
        assert!(!same_grid(&a, &b[..1]), "fewer segments");
        let shifted = [seg("b/0.m4s", 0, 5_000), seg("b/1.m4s", 5_000, 5_000)];
        assert!(!same_grid(&a, &shifted), "same count, different cuts");
        assert!(same_grid(&[], &[]));
    }

    #[test]
    fn empty_playlist_has_zero_duration_and_no_segments() {
        let p = parse_media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-ENDLIST\n",
            "http://pharos.local/main.m3u8",
        )
        .unwrap();
        assert_eq!(duration_ms(&p), 0);
        assert_eq!(segment_at(&p, 0), None);
        assert_eq!(range_of(&p, 0), None);
    }

    proptest! {
        #[test]
        fn timeline_butt_joins_and_maps_every_playhead_time(
            millis in prop::collection::vec(500u32..=10_000u32, 1..400),
            probes in prop::collection::vec(0.0f64..1.0f64, 1..64),
        ) {
            let mut body = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:10\n");
            let mut sum_secs = 0.0f64;
            for (i, &ms) in millis.iter().enumerate() {
                let secs = f64::from(ms) / 1000.0;
                sum_secs += secs;
                body.push_str(&format!("#EXTINF:{secs:.3},\n{i}.m4s\n"));
            }
            body.push_str("#EXT-X-ENDLIST\n");

            let playlist = parse_media(&body, "http://pharos.local/videos/1/vp9/main.m3u8").unwrap();
            prop_assert_eq!(playlist.segments.len(), millis.len());

            let total_ms = duration_ms(&playlist);
            let expected_total_ms = (sum_secs * 1000.0).round() as u64;
            prop_assert_eq!(total_ms, expected_total_ms);

            for i in 0..playlist.segments.len() {
                let (start, end) = range_of(&playlist, i).unwrap();
                prop_assert!(start < end);
                if i == 0 {
                    prop_assert_eq!(start, 0);
                } else {
                    let (_, prev_end) = range_of(&playlist, i - 1).unwrap();
                    prop_assert_eq!(start, prev_end);
                }
                prop_assert_eq!(segment_at(&playlist, start), Some(i));
                prop_assert_eq!(segment_at(&playlist, start + (end - start) / 2), Some(i));
                prop_assert_eq!(segment_at(&playlist, end - 1), Some(i));
            }

            for frac in probes {
                let at_ms = ((frac * (total_ms as f64)).floor() as u64).min(total_ms - 1);
                let idx = segment_at(&playlist, at_ms).unwrap();
                let (start, end) = range_of(&playlist, idx).unwrap();
                prop_assert!(start <= at_ms && at_ms < end);
            }

            prop_assert_eq!(segment_at(&playlist, total_ms), None);
            prop_assert_eq!(segment_at(&playlist, total_ms + 1), None);
        }
    }
}
