//! Stall watchdog: small-gap jumping and stall nudges (spec 031 slice 3,
//! T031-15, V701, V702).
//!
//! Pure: the adapter samples the media element on a timer and executes the
//! verdict. Gaps are judged on the element's `buffered`, which is the
//! intersection of the audio and video tracks.

/// A play head unmoved this long while playing is a stall (hls.js
/// `detectStallWithCurrentTimeMs`).
const STALL_MS: u64 = 1_250;

/// A stall with data buffered past the play head waits this long before a
/// nudge (hls.js `highBufferWatchdogPeriod`).
const NUDGE_AFTER_MS: u64 = 2_000;

/// Nudge step: nudge `n` moves the play head `n * NUDGE_STEP_MS` ahead
/// (hls.js `nudgeOffset`).
const NUDGE_STEP_MS: u64 = 100;

/// Jumps or nudges in one stall before giving up (hls.js `nudgeMaxRetry`).
const MAX_ATTEMPTS: u32 = 3;

/// A hole no wider than this counts as no data ahead (hls.js `maxBufferHole`).
const HOLE_MS: u64 = 100;

/// Widest gap the watchdog jumps. Wider gaps are missing segments, which the
/// planner fetches; jumping them would skip content. Provisional.
const MAX_JUMP_MS: u64 = 1_000;

/// Lands this far past a gap's end (hls.js `skipBufferHolePadding`).
const JUMP_PADDING_MS: u64 = 100;

/// One sample of the media element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample<'a> {
    pub now_ms: u64,
    pub current_ms: u64,
    /// Paused, ended or a zero playback rate: the play head is meant to be
    /// still.
    pub held: bool,
    /// A seek is under way: the watchdog's own move, or one that ends the stall.
    pub seeking: bool,
    /// The element's buffered ranges, start inclusive, end exclusive.
    pub buffered: &'a [(u64, u64)],
}

/// What the watchdog decided on a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Set `currentTime` to `to_ms`, past a gap `gap_ms` wide.
    Jump {
        from_ms: u64,
        to_ms: u64,
        gap_ms: u64,
        attempt: u32,
    },
    /// Set `currentTime` to `to_ms` with `ahead_ms` buffered past the play head.
    Nudge {
        from_ms: u64,
        to_ms: u64,
        ahead_ms: u64,
        attempt: u32,
    },
    /// No data at or just past the play head: the planner's job. Reported once
    /// per stall.
    Starved { at_ms: u64, stalled_ms: u64 },
    /// Jumps and nudges ran out with the play head still unmoved.
    GiveUp {
        at_ms: u64,
        stalled_ms: u64,
        attempts: u32,
    },
    /// The play head moved again after a stall that was reported.
    Recovered {
        at_ms: u64,
        stalled_ms: u64,
        attempts: u32,
    },
}

impl Verdict {
    /// Stable log and telemetry token.
    pub fn label(self) -> &'static str {
        match self {
            Self::Jump { .. } => "jump",
            Self::Nudge { .. } => "nudge",
            Self::Starved { .. } => "starved",
            Self::GiveUp { .. } => "give_up",
            Self::Recovered { .. } => "recovered",
        }
    }
}

/// One stall: from the first sample the play head stopped moving.
#[derive(Debug, Clone, Copy)]
struct Episode {
    since_ms: u64,
    last_action_ms: u64,
    attempts: u32,
    reported: bool,
    gave_up: bool,
}

/// A play head this close to where the watchdog moved it got there by the
/// watchdog's own move, not by playing.
const LANDED_MS: u64 = 50;

/// Watches the play head for stalls across samples.
#[derive(Debug, Clone, Default)]
pub struct Watchdog {
    last_ms: Option<u64>,
    episode: Option<Episode>,
    /// Where the last jump or nudge moved the play head, until it lands.
    moved_to_ms: Option<u64>,
}

impl Watchdog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Judges one sample; at most one verdict per sample.
    pub fn observe(&mut self, s: Sample<'_>) -> Option<Verdict> {
        if let Some(to) = self.moved_to_ms {
            if s.seeking {
                return None;
            }
            if s.current_ms.abs_diff(to) <= LANDED_MS {
                self.moved_to_ms = None;
                self.last_ms = Some(s.current_ms);
            }
        }
        let moved = self.last_ms.is_some_and(|last| last != s.current_ms);
        self.last_ms = Some(s.current_ms);
        if s.held || s.seeking || moved {
            self.moved_to_ms = None;
            let ended = self.episode.take()?;
            return (ended.reported && moved && !s.held && !s.seeking).then(|| {
                Verdict::Recovered {
                    at_ms: s.current_ms,
                    stalled_ms: s.now_ms.saturating_sub(ended.since_ms),
                    attempts: ended.attempts,
                }
            });
        }

        let ep = self.episode.get_or_insert(Episode {
            since_ms: s.now_ms,
            last_action_ms: s.now_ms,
            attempts: 0,
            reported: false,
            gave_up: false,
        });
        let stalled_ms = s.now_ms.saturating_sub(ep.since_ms);
        if stalled_ms < STALL_MS || ep.gave_up {
            return None;
        }
        // Since the stall began, or since the last move if there was one.
        let waited_ms = if ep.attempts == 0 {
            stalled_ms
        } else {
            s.now_ms.saturating_sub(ep.last_action_ms)
        };

        let at = s.current_ms;
        let next_start = s
            .buffered
            .iter()
            .map(|&(start, _)| start)
            .filter(|&start| start > at)
            .min()
            .filter(|&start| start - at <= MAX_JUMP_MS);
        let ahead_ms = s
            .buffered
            .iter()
            .find(|&&(start, end)| start <= at && at < end)
            .map_or(0, |&(_, end)| end - at);

        let verdict = if let Some(start) = next_start {
            if waited_ms < STALL_MS {
                return None;
            }
            self.attempt(at, stalled_ms, |attempt| Verdict::Jump {
                from_ms: at,
                to_ms: start.saturating_add(JUMP_PADDING_MS),
                gap_ms: start - at,
                attempt,
            })
        } else if ahead_ms > HOLE_MS {
            if waited_ms < NUDGE_AFTER_MS {
                return None;
            }
            self.attempt(at, stalled_ms, |attempt| Verdict::Nudge {
                from_ms: at,
                to_ms: at.saturating_add(u64::from(attempt) * NUDGE_STEP_MS),
                ahead_ms,
                attempt,
            })
        } else if ep.reported {
            return None;
        } else {
            Verdict::Starved {
                at_ms: at,
                stalled_ms,
            }
        };
        if let Some(ep) = self.episode.as_mut() {
            ep.reported = true;
            ep.last_action_ms = s.now_ms;
        }
        if let Verdict::Jump { to_ms, .. } | Verdict::Nudge { to_ms, .. } = verdict {
            self.moved_to_ms = Some(to_ms);
        }
        Some(verdict)
    }

    /// The next move, or a give-up once [`MAX_ATTEMPTS`] moves have not helped.
    fn attempt(&mut self, at: u64, stalled_ms: u64, mv: impl FnOnce(u32) -> Verdict) -> Verdict {
        let Some(ep) = self.episode.as_mut() else {
            return Verdict::GiveUp {
                at_ms: at,
                stalled_ms,
                attempts: 0,
            };
        };
        if ep.attempts >= MAX_ATTEMPTS {
            ep.gave_up = true;
            return Verdict::GiveUp {
                at_ms: at,
                stalled_ms,
                attempts: ep.attempts,
            };
        }
        ep.attempts += 1;
        mv(ep.attempts)
    }
}

#[cfg(test)]
mod tests {
    use super::{Sample, Verdict, Watchdog};

    fn sample(now_ms: u64, current_ms: u64, buffered: &[(u64, u64)]) -> Sample<'_> {
        Sample {
            now_ms,
            current_ms,
            held: false,
            seeking: false,
            buffered,
        }
    }

    /// Samples every 250 ms from `from` to `to` (inclusive) at a still play
    /// head; the verdicts with their times.
    fn run(
        w: &mut Watchdog,
        from: u64,
        to: u64,
        at: u64,
        buf: &[(u64, u64)],
    ) -> Vec<(u64, Verdict)> {
        (from..=to)
            .step_by(250)
            .filter_map(|now| w.observe(sample(now, at, buf)).map(|v| (now, v)))
            .collect()
    }

    #[test]
    fn a_moving_play_head_never_stalls() {
        let mut w = Watchdog::new();
        let buf = [(0, 60_000)];
        for i in 0..100 {
            assert_eq!(w.observe(sample(i * 250, i * 250, &buf)), None);
        }
    }

    #[test]
    fn a_small_gap_is_jumped_after_the_stall_threshold() {
        let mut w = Watchdog::new();
        let buf = [(0, 10_000), (10_400, 60_000)];
        let got = run(&mut w, 0, 1_500, 10_000, &buf);
        assert_eq!(
            got,
            vec![(
                1_250,
                Verdict::Jump {
                    from_ms: 10_000,
                    to_ms: 10_500,
                    gap_ms: 400,
                    attempt: 1,
                }
            )]
        );
        // The watchdog's own seek, then landing, is not playing again.
        let seeking = Sample {
            seeking: true,
            ..sample(1_500, 10_000, &buf)
        };
        assert_eq!(w.observe(seeking), None);
        assert_eq!(w.observe(sample(1_750, 10_500, &buf)), None);
        assert_eq!(
            w.observe(sample(2_000, 10_750, &buf)),
            Some(Verdict::Recovered {
                at_ms: 10_750,
                stalled_ms: 2_000,
                attempts: 1,
            })
        );
    }

    // Chromium stops short of a range's end: a hole just past the play head
    // is jumped although the play head is still inside buffered data.
    #[test]
    fn a_hole_just_ahead_of_a_play_head_stuck_short_of_it_is_jumped() {
        let mut w = Watchdog::new();
        let buf = [(67, 7_000), (7_400, 30_033)];
        let got = run(&mut w, 0, 1_500, 6_871, &buf);
        assert_eq!(
            got,
            vec![(
                1_250,
                Verdict::Jump {
                    from_ms: 6_871,
                    to_ms: 7_500,
                    gap_ms: 529,
                    attempt: 1,
                }
            )]
        );
    }

    // A nudge that lands and sticks is the same stall, so the attempts add up
    // to a give-up instead of nudging forever.
    #[test]
    fn nudges_that_land_and_stick_add_up_to_a_give_up() {
        let mut w = Watchdog::new();
        let buf = [(0, 60_000)];
        let mut at = 30_000;
        let mut got = Vec::new();
        for now in (0..=20_000).step_by(250) {
            if let Some(v) = w.observe(sample(now, at, &buf)) {
                if let Verdict::Nudge { to_ms, .. } = v {
                    at = to_ms;
                }
                got.push((now, v.label()));
            }
        }
        assert_eq!(
            got,
            [
                (2_000, "nudge"),
                (4_000, "nudge"),
                (6_000, "nudge"),
                (8_000, "give_up"),
            ]
        );
    }

    #[test]
    fn a_wide_gap_is_left_to_the_planner_and_reported_once() {
        let mut w = Watchdog::new();
        let buf = [(0, 10_000), (16_000, 60_000)];
        let got = run(&mut w, 0, 20_000, 10_000, &buf);
        assert_eq!(
            got,
            vec![(
                1_250,
                Verdict::Starved {
                    at_ms: 10_000,
                    stalled_ms: 1_250,
                }
            )]
        );
    }

    #[test]
    fn a_stall_with_data_ahead_is_nudged_then_given_up() {
        let mut w = Watchdog::new();
        let buf = [(0, 60_000)];
        let got = run(&mut w, 0, 20_000, 30_000, &buf);
        let labels: Vec<(u64, &str)> = got.iter().map(|&(t, v)| (t, v.label())).collect();
        assert_eq!(
            labels,
            [
                (2_000, "nudge"),
                (4_000, "nudge"),
                (6_000, "nudge"),
                (8_000, "give_up"),
            ]
        );
        assert!(matches!(
            got[2].1,
            Verdict::Nudge {
                to_ms: 30_300,
                attempt: 3,
                ..
            }
        ));
    }

    #[test]
    fn a_held_play_head_is_not_a_stall_and_ends_one() {
        let mut w = Watchdog::new();
        let buf = [(0, 60_000)];
        for now in (0..10_000).step_by(250) {
            let s = Sample {
                held: true,
                ..sample(now, 5_000, &buf)
            };
            assert_eq!(w.observe(s), None);
        }
        let got = run(&mut w, 10_000, 11_000, 5_000, &buf);
        assert_eq!(got, Vec::new());
    }

    #[test]
    fn data_ending_at_the_play_head_is_starved_not_nudged() {
        let mut w = Watchdog::new();
        let buf = [(0, 30_050)];
        let got = run(&mut w, 0, 10_000, 30_000, &buf);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.label(), "starved");
    }

    #[test]
    fn verdict_labels_are_distinct() {
        let labels = [
            Verdict::Jump {
                from_ms: 0,
                to_ms: 0,
                gap_ms: 0,
                attempt: 0,
            },
            Verdict::Nudge {
                from_ms: 0,
                to_ms: 0,
                ahead_ms: 0,
                attempt: 0,
            },
            Verdict::Starved {
                at_ms: 0,
                stalled_ms: 0,
            },
            Verdict::GiveUp {
                at_ms: 0,
                stalled_ms: 0,
                attempts: 0,
            },
            Verdict::Recovered {
                at_ms: 0,
                stalled_ms: 0,
                attempts: 0,
            },
        ]
        .map(Verdict::label);
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len());
    }
}
