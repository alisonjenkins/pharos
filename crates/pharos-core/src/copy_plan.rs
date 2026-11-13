//! Keyframe-aligned segment planner for the `h264copy` HLS surface (ADR-0123).
//!
//! Unlike the encoded surfaces on [`crate::segment_grid`], a stream-copied
//! segment can only start on a source video keyframe. This module groups a
//! source's keyframe presentation times into segments of around
//! [`COPY_TARGET_SECONDS`] (never shorter than [`COPY_MIN_SECONDS`] except the
//! tail, and never longer than [`COPY_MAX_SECONDS`]), or refuses the source
//! when its keyframe spacing cannot meet those bounds.

use serde::{Deserialize, Serialize};

/// Nominal target length of a copied HLS segment, in seconds.
pub const COPY_TARGET_SECONDS: u32 = 6;

/// Minimum length of a non-final copied HLS segment, in seconds.
pub const COPY_MIN_SECONDS: u32 = 4;

/// Maximum length of any copied HLS segment, in seconds.
pub const COPY_MAX_SECONDS: u32 = 12;

/// One half-open `[start, end)` segment range on the source's video timescale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySegment {
    pub start: i64,
    pub end: i64,
}

/// The complete keyframe-aligned segment plan for one source video stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyPlan {
    pub timescale: u32,
    pub segments: Vec<CopySegment>,
}

impl CopyPlan {
    /// Index of the segment containing `t` (ticks); `t` before the first start -> `Some(0)`; `t >= last end` -> `None`.
    pub fn segment_at(&self, t: i64) -> Option<usize> {
        let first = self.segments.first()?;
        if t < first.start {
            return Some(0);
        }
        let idx = self.segments.partition_point(|seg| seg.end <= t);
        (idx < self.segments.len()).then_some(idx)
    }

    /// Longest segment duration in whole seconds, rounded UP (for `#EXT-X-TARGETDURATION`). 0 for an empty plan.
    pub fn target_duration_secs(&self) -> u32 {
        if self.timescale == 0 {
            return 0;
        }
        let ts = u128::from(self.timescale);
        self.segments
            .iter()
            .map(|seg| {
                let ticks = i128::from(seg.end)
                    .saturating_sub(i128::from(seg.start))
                    .max(0) as u128;
                u32::try_from(ticks.div_ceil(ts)).unwrap_or(u32::MAX)
            })
            .max()
            .unwrap_or(0)
    }

    /// Duration of segment `i` in seconds as `f64` (for `#EXTINF`). `None` when out of range.
    pub fn duration_secs(&self, i: usize) -> Option<f64> {
        if self.timescale == 0 {
            return None;
        }
        let seg = self.segments.get(i)?;
        let ticks = (i128::from(seg.end) - i128::from(seg.start)) as f64;
        Some(ticks / f64::from(self.timescale))
    }
}

/// Reason a source's video keyframe timeline cannot be served on `h264copy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum PlanRefusal {
    #[error("no keyframes")]
    NoKeyframes,
    #[error("timescale is zero")]
    ZeroTimescale,
    #[error("keyframes not strictly ascending at index {index}: {prev} then {next}")]
    Unsorted { index: usize, prev: i64, next: i64 },
    #[error("end {end} is not after the last keyframe {last}")]
    EndBeforeLastKeyframe { end: i64, last: i64 },
    #[error("no keyframe between {min_secs}s and {max_secs}s after {at} ticks (next keyframe at {next:?})")]
    LongGap {
        at: i64,
        next: Option<i64>,
        min_secs: u32,
        max_secs: u32,
    },
}

/// Greedy from the first keyframe: each segment ends on the keyframe in
/// `[min, max]` after its start closest to the target (the earlier on a tie).
/// The rest becomes the last segment once it fits in `max` and cannot be split
/// into two parts of at least `min`.
///
/// `keyframes`: presentation times of every video keyframe in ascending order (first = the first video frame).
/// `end`: presentation end of the video (last frame pts + its duration), ticks.
pub fn plan(keyframes: &[i64], end: i64, timescale: u32) -> Result<CopyPlan, PlanRefusal> {
    if timescale == 0 {
        return Err(PlanRefusal::ZeroTimescale);
    }
    let (&first, &last) = match (keyframes.first(), keyframes.last()) {
        (Some(f), Some(l)) => (f, l),
        _ => return Err(PlanRefusal::NoKeyframes),
    };
    for (i, pair) in keyframes.windows(2).enumerate() {
        let (prev, next) = (pair[0], pair[1]);
        if prev >= next {
            return Err(PlanRefusal::Unsorted {
                index: i + 1,
                prev,
                next,
            });
        }
    }
    if end <= last {
        return Err(PlanRefusal::EndBeforeLastKeyframe { end, last });
    }

    let ts = i128::from(timescale);
    let min = i128::from(COPY_MIN_SECONDS) * ts;
    let target = i128::from(COPY_TARGET_SECONDS) * ts;
    let max = i128::from(COPY_MAX_SECONDS) * ts;
    let end128 = i128::from(end);

    let mut segments = Vec::new();
    let mut s = first;
    loop {
        let s128 = i128::from(s);
        let fits = end128 - s128 <= max;
        let win_min = s128 + min;
        let win_target = s128 + target;
        let win_max = s128 + max;
        // A tail that fits one segment is still split when that leaves both
        // parts at least `min` long, so it is not up to twice the target.
        // `keyframes` is sorted, so the window is one contiguous slice.
        let lo = keyframes.partition_point(|&k| i128::from(k) < win_min);
        let hi = keyframes.partition_point(|&k| i128::from(k) <= win_max);
        let chosen = keyframes[lo..hi]
            .iter()
            .copied()
            .filter(|&k| !fits || end128 - i128::from(k) >= min)
            .min_by_key(|&k| (i128::from(k) - win_target).abs());

        if fits && chosen.is_none() {
            segments.push(CopySegment { start: s, end });
            break;
        }
        let Some(k) = chosen else {
            let next = keyframes.iter().copied().find(|&k| k > s);
            return Err(PlanRefusal::LongGap {
                at: s,
                next,
                min_secs: COPY_MIN_SECONDS,
                max_secs: COPY_MAX_SECONDS,
            });
        };
        segments.push(CopySegment { start: s, end: k });
        s = k;
    }

    Ok(CopyPlan {
        timescale,
        segments,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn regular_two_second_keyframes_produce_six_second_segments() {
        let keyframes: Vec<i64> = (0..30).map(|i| i * 2_000).collect();
        let got = plan(&keyframes, 60_000, 1_000).unwrap();
        assert_eq!(got.timescale, 1_000);
        assert_eq!(got.segments.len(), 10);
        for (i, seg) in got.segments.iter().enumerate() {
            let expected_start = (i as i64) * 6_000;
            assert_eq!(
                *seg,
                CopySegment {
                    start: expected_start,
                    end: expected_start + 6_000,
                }
            );
            assert_eq!(got.duration_secs(i), Some(6.0));
        }
        assert_eq!(got.duration_secs(10), None);
        assert_eq!(got.target_duration_secs(), 6);
    }

    #[test]
    fn irregular_keyframes_match_hand_computed_plan() {
        let keyframes = [
            0, 2_500, 7_100, 13_300, 14_000, 21_900, 28_000, 33_700, 41_000, 47_500, 52_000, 59_000,
        ];
        let got = plan(&keyframes, 60_000, 1_000).unwrap();
        let expected = [
            CopySegment {
                start: 0,
                end: 7_100,
            },
            CopySegment {
                start: 7_100,
                end: 13_300,
            },
            CopySegment {
                start: 13_300,
                end: 21_900,
            },
            CopySegment {
                start: 21_900,
                end: 28_000,
            },
            CopySegment {
                start: 28_000,
                end: 33_700,
            },
            CopySegment {
                start: 33_700,
                end: 41_000,
            },
            CopySegment {
                start: 41_000,
                end: 47_500,
            },
            CopySegment {
                start: 47_500,
                end: 52_000,
            },
            CopySegment {
                start: 52_000,
                end: 60_000,
            },
        ];
        assert_eq!(got.segments, expected);
        assert_eq!(got.target_duration_secs(), 9);
    }

    #[test]
    fn tie_breaks_to_earlier_keyframe() {
        let keyframes = [0, 5_000, 7_000, 12_000];
        let got = plan(&keyframes, 15_000, 1_000).unwrap();
        assert_eq!(
            got.segments,
            [
                CopySegment {
                    start: 0,
                    end: 5_000
                },
                CopySegment {
                    start: 5_000,
                    end: 15_000
                },
            ]
        );
    }

    #[test]
    fn thirteen_second_gap_is_refused_with_long_gap() {
        let keyframes = [0, 6_000, 19_000];
        assert_eq!(
            plan(&keyframes, 24_000, 1_000),
            Err(PlanRefusal::LongGap {
                at: 6_000,
                next: Some(19_000),
                min_secs: 4,
                max_secs: 12,
            })
        );

        let trailing = [0, 2_000];
        assert_eq!(
            plan(&trailing, 20_000, 1_000),
            Err(PlanRefusal::LongGap {
                at: 0,
                next: Some(2_000),
                min_secs: 4,
                max_secs: 12,
            })
        );

        let single_far_end = [0];
        assert_eq!(
            plan(&single_far_end, 13_000, 1_000),
            Err(PlanRefusal::LongGap {
                at: 0,
                next: None,
                min_secs: 4,
                max_secs: 12,
            })
        );
    }

    #[test]
    fn validation_rejects_empty_zero_timescale_unsorted_and_bad_end() {
        assert_eq!(plan(&[], 10_000, 1_000), Err(PlanRefusal::NoKeyframes));
        assert_eq!(plan(&[0], 10_000, 0), Err(PlanRefusal::ZeroTimescale));
        assert_eq!(
            plan(&[0, 5_000, 5_000, 10_000], 12_000, 1_000),
            Err(PlanRefusal::Unsorted {
                index: 2,
                prev: 5_000,
                next: 5_000,
            })
        );
        assert_eq!(
            plan(&[0, 5_000, 4_000, 10_000], 12_000, 1_000),
            Err(PlanRefusal::Unsorted {
                index: 2,
                prev: 5_000,
                next: 4_000,
            })
        );
        assert_eq!(
            plan(&[0, 6_000], 6_000, 1_000),
            Err(PlanRefusal::EndBeforeLastKeyframe {
                end: 6_000,
                last: 6_000,
            })
        );
        assert_eq!(
            plan(&[0, 6_000], 5_999, 1_000),
            Err(PlanRefusal::EndBeforeLastKeyframe {
                end: 5_999,
                last: 6_000,
            })
        );
    }

    #[test]
    fn segment_at_handles_boundaries_and_out_of_range() {
        let p = CopyPlan {
            timescale: 1_000,
            segments: vec![
                CopySegment {
                    start: 1_000,
                    end: 7_000,
                },
                CopySegment {
                    start: 7_000,
                    end: 13_500,
                },
            ],
        };
        assert_eq!(p.segment_at(0), Some(0));
        assert_eq!(p.segment_at(999), Some(0));
        assert_eq!(p.segment_at(1_000), Some(0));
        assert_eq!(p.segment_at(6_999), Some(0));
        assert_eq!(p.segment_at(7_000), Some(1));
        assert_eq!(p.segment_at(13_499), Some(1));
        assert_eq!(p.segment_at(13_500), None);
        assert_eq!(p.segment_at(20_000), None);

        let empty = CopyPlan {
            timescale: 1_000,
            segments: Vec::new(),
        };
        assert_eq!(empty.segment_at(0), None);
        assert_eq!(empty.target_duration_secs(), 0);
    }

    #[test]
    fn target_duration_secs_rounds_up_partial_seconds() {
        let p = CopyPlan {
            timescale: 1_000,
            segments: vec![
                CopySegment {
                    start: 0,
                    end: 6_000,
                },
                CopySegment {
                    start: 6_000,
                    end: 12_001,
                },
            ],
        };
        assert_eq!(p.target_duration_secs(), 7);
    }

    proptest! {
        #[test]
        fn dense_keyframes_always_tile_within_bounds(
            k0 in -5_000i64..=5_000i64,
            gaps in prop::collection::vec(1i64..=5_000i64, 0..80),
            tail in 1i64..=5_000i64,
        ) {
            let mut keyframes = Vec::with_capacity(gaps.len() + 1);
            let mut cur = k0;
            keyframes.push(cur);
            for g in gaps {
                cur += g;
                keyframes.push(cur);
            }
            let end = cur + tail;
            let got = plan(&keyframes, end, 1_000).unwrap();

            prop_assert!(!got.segments.is_empty());
            prop_assert_eq!(got.segments.first().unwrap().start, k0);
            prop_assert_eq!(got.segments.last().unwrap().end, end);

            let min_ticks = i64::from(COPY_MIN_SECONDS) * 1_000;
            let max_ticks = i64::from(COPY_MAX_SECONDS) * 1_000;
            let n = got.segments.len();

            for (i, seg) in got.segments.iter().enumerate() {
                prop_assert!(keyframes.binary_search(&seg.start).is_ok());
                let len = seg.end - seg.start;
                if i + 1 < n {
                    prop_assert!(
                        (min_ticks..=max_ticks).contains(&len),
                        "non-last segment {i} length {len} outside [{min_ticks}, {max_ticks}]"
                    );
                    prop_assert_eq!(seg.end, got.segments[i + 1].start);
                } else {
                    prop_assert!(
                        len > 0 && len <= max_ticks,
                        "last segment length {len} outside (0, {max_ticks}]"
                    );
                }
                prop_assert_eq!(got.segment_at(seg.start), Some(i));
                prop_assert_eq!(got.segment_at(seg.end - 1), Some(i));
            }
            prop_assert_eq!(got.segment_at(end), None);
        }
    }
}
