//! Auto quality (ADR-0056): picks a bitrate cap for a video from what the
//! connection delivers, so a viewer on a slow link stops stalling without
//! choosing a quality by hand.
//!
//! [`Watch`] collects fragment downloads and stalls; [`decide`] is the pure
//! rule over them. A decision is a new cap for the next negotiation, never a
//! mid-stream switch: the server turns a source above the cap into a capped
//! transcode.

/// The caps auto quality steps between, highest first. These are the
/// manual picker's choices (`MAX_BITRATE_CHOICES`), so a remembered cap is
/// always one the server has seen.
pub const RUNGS: [u64; 4] = [8_000_000, 4_000_000, 2_000_000, 1_000_000];

/// Where the last cap auto quality settled on is kept, in bits per second;
/// absent or `0` is none. It is where the next video starts.
pub const AUTO_RUNG_KEY: &str = "pharos.player.auto_rung";

/// The cap a stored [`AUTO_RUNG_KEY`] value holds; anything that is not
/// one of the [`RUNGS`] is none.
pub fn parse_rung(stored: &str) -> u64 {
    stored
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|bps| RUNGS.contains(bps))
        .unwrap_or(0)
}

/// The cap to negotiate with: the lower of the viewer's manual maximum and
/// the automatic one, where 0 is no limit.
pub fn effective_cap(manual: u64, auto: u64) -> u64 {
    match (manual, auto) {
        (0, auto) => auto,
        (manual, 0) => manual,
        (manual, auto) => manual.min(auto),
    }
}

/// A stall older than this no longer counts.
const STALL_WINDOW_MS: u64 = 30_000;
/// Stalls inside the window that step the quality down.
const STALLS_TO_STEP_DOWN: usize = 2;
/// Two `waiting`/`stalled` events this close are one stall.
const STALL_MERGE_MS: u64 = 2_000;
/// After a start or a change the buffer is filling; nothing is decided.
const SETTLE_MS: u64 = 15_000;
/// The link must keep the headroom this long before stepping up.
const HEADROOM_HOLD_MS: u64 = 60_000;
/// Downloads needed before the estimate is believed.
const MIN_SAMPLES: u32 = 3;
/// Smaller downloads say little about the link.
const MIN_SAMPLE_BYTES: u64 = 100_000;
/// Downloads faster than this are cache hits, not a measure of the link.
const MIN_LOAD_MS: u64 = 50;
/// No single download is believed above this, in bits per second.
const MAX_LINK_BPS: u64 = 200_000_000;
/// Weight of the newest download in the throughput average, in percent.
const EWMA_NEW_PERCENT: u64 = 30;
/// Step down below this multiple of the stream's bitrate, in tenths.
const DOWN_BELOW_TENTHS: u64 = 13;
/// Step up when the link carries this multiple of the next cap, in tenths.
const UP_ABOVE_TENTHS: u64 = 20;

/// Why the quality changed, as the `reason=` of the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Stalls,
    LowEstimate,
    Headroom,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stalls => "stalls",
            Self::LowEstimate => "low_estimate",
            Self::Headroom => "headroom",
        }
    }
}

/// Everything [`decide`] looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Inputs {
    /// The auto cap now in force; 0 is none.
    pub cap: u64,
    /// The viewer's manual maximum, a hard ceiling; 0 is none.
    pub ceiling: u64,
    /// The source's own bitrate, once seen uncapped.
    pub source_bps: Option<u64>,
    /// What the stream now plays at: the cap, or the measured bitrate.
    pub current_bps: Option<u64>,
    /// Smoothed download throughput, once enough downloads were seen.
    pub estimate_bps: Option<u64>,
    /// Stalls inside the window.
    pub stalls: usize,
    /// How long the headroom for the next step up has held.
    pub headroom_held_ms: u64,
    /// Still settling after a start or a change.
    pub settling: bool,
}

/// What [`decide`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Keep,
    /// Negotiate with this auto cap; 0 lifts the auto cap.
    Change {
        cap: u64,
        to_bps: u64,
        reason: Reason,
    },
}

/// The cap one step above `cap`, and the bitrate it plays at: a higher rung
/// within the ceiling, or no auto cap (0) once a rung reaches the source.
/// `None` at the top, or when the source is unknown at the top rung.
pub fn up_target(cap: u64, ceiling: u64, source_bps: Option<u64>) -> Option<(u64, u64)> {
    if cap == 0 {
        return None;
    }
    let next = RUNGS
        .iter()
        .rev()
        .copied()
        .find(|&r| r > cap && (ceiling == 0 || r <= ceiling));
    match (next, source_bps) {
        (Some(r), Some(source)) if r >= source => Some((0, source)),
        (Some(r), _) => Some((r, r)),
        (None, Some(source)) if ceiling == 0 || source <= ceiling => Some((0, source)),
        (None, _) => None,
    }
}

/// Keep, step down or step up.
pub fn decide(i: &Inputs) -> Decision {
    let Some(current) = i.current_bps.filter(|_| !i.settling) else {
        return Decision::Keep;
    };
    let stalled = i.stalls >= STALLS_TO_STEP_DOWN;
    let slow = i
        .estimate_bps
        .is_some_and(|e| e.saturating_mul(10) < current.saturating_mul(DOWN_BELOW_TENTHS));
    if stalled || slow {
        let reason = if stalled {
            Reason::Stalls
        } else {
            Reason::LowEstimate
        };
        let fits = |r: u64| {
            i.estimate_bps.map_or(true, |e| {
                r.saturating_mul(DOWN_BELOW_TENTHS) <= e.saturating_mul(10)
            })
        };
        let lower = || RUNGS.iter().copied().filter(|&r| r < current);
        // Rungs run highest first: the best one the link carries, else the
        // lowest there is.
        let to = lower().find(|&r| fits(r)).or_else(|| lower().next_back());
        return to.map_or(Decision::Keep, |to| Decision::Change {
            cap: to,
            to_bps: to,
            reason,
        });
    }
    match up_target(i.cap, i.ceiling, i.source_bps) {
        Some((cap, to_bps)) if i.headroom_held_ms >= HEADROOM_HOLD_MS => Decision::Change {
            cap,
            to_bps,
            reason: Reason::Headroom,
        },
        _ => Decision::Keep,
    }
}

/// A change [`Watch::evaluate`] made, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub cap: u64,
    pub from_bps: u64,
    pub to_bps: u64,
    pub reason: Reason,
    pub estimate_bps: u64,
}

/// One video's connection history, kept across the reloads its own
/// decisions cause.
#[derive(Debug, Clone)]
pub struct Watch {
    ceiling: u64,
    cap: u64,
    source_bps: Option<u64>,
    stream_bps: Option<u64>,
    estimate_bps: Option<u64>,
    samples: u32,
    stalls: Vec<u64>,
    quiet_until: u64,
    headroom_since: Option<u64>,
    seeking_since: Option<u64>,
}

impl Watch {
    /// A watch starting at `now` under the manual `ceiling`, from the
    /// remembered `cap` (both 0 for none).
    pub fn new(ceiling: u64, cap: u64, now: u64) -> Self {
        let cap = if cap > 0 && ceiling > 0 {
            cap.min(ceiling)
        } else {
            cap
        };
        Self {
            ceiling,
            cap,
            source_bps: None,
            stream_bps: None,
            estimate_bps: None,
            samples: 0,
            stalls: Vec::new(),
            quiet_until: now.saturating_add(SETTLE_MS),
            headroom_since: None,
            seeking_since: None,
        }
    }

    /// The auto cap now in force; 0 is none.
    pub fn cap(&self) -> u64 {
        self.cap
    }

    /// A fragment of `bytes` took `load_ms` to download and holds
    /// `media_ms` of video.
    pub fn fragment(&mut self, bytes: u64, load_ms: u64, media_ms: u64) {
        // A cache hit answers in a millisecond; that says nothing of the link.
        if bytes < MIN_SAMPLE_BYTES || load_ms < MIN_LOAD_MS || media_ms == 0 {
            return;
        }
        let link = (bytes.saturating_mul(8000) / load_ms).min(MAX_LINK_BPS);
        let stream = bytes.saturating_mul(8000) / media_ms;
        let blend = |old: Option<u64>, new: u64| {
            old.map_or(new, |old| {
                (old.saturating_mul(100 - EWMA_NEW_PERCENT) + new.saturating_mul(EWMA_NEW_PERCENT))
                    / 100
            })
        };
        self.estimate_bps = Some(blend(self.estimate_bps, link));
        self.stream_bps = Some(blend(self.stream_bps, stream));
        self.samples = self.samples.saturating_add(1);
        // Only an uncapped stream is the source's own bitrate.
        if self.cap == 0 {
            self.source_bps = self.stream_bps;
        }
    }

    /// The video ran out of data at `now`.
    pub fn stalled(&mut self, now: u64) {
        if self.seeking_since.is_some() {
            return;
        }
        let merged = self
            .stalls
            .last()
            .is_some_and(|&last| now.saturating_sub(last) < STALL_MERGE_MS);
        if now < self.quiet_until || merged {
            return;
        }
        self.stalls.push(now);
        self.headroom_since = None;
    }

    /// A seek started at `now`; every `waiting` until it finishes is its own.
    pub fn seeking(&mut self, now: u64) {
        self.seeking_since = Some(now);
    }

    /// The seek finished; the `waiting`s it caused were no stalls.
    pub fn seeked(&mut self) {
        self.seeking_since = None;
    }

    /// A player (re)started at `now`: its buffer is filling.
    pub fn restarted(&mut self, now: u64) {
        self.quiet_until = now.saturating_add(SETTLE_MS);
        self.seeking_since = None;
    }

    /// Decides at `now`; applies and returns a change.
    pub fn evaluate(&mut self, now: u64) -> Option<Change> {
        self.stalls
            .retain(|&t| now.saturating_sub(t) <= STALL_WINDOW_MS);
        let settling = now < self.quiet_until;
        let current = match (self.cap, self.stream_bps) {
            (0, stream) => stream,
            (cap, Some(stream)) => Some(cap.min(stream)),
            (_, None) => None,
        };
        let estimate = self.estimate_bps.filter(|_| self.samples >= MIN_SAMPLES);
        let headroom = !settling
            && up_target(self.cap, self.ceiling, self.source_bps).is_some_and(|(_, to)| {
                estimate.is_some_and(|e| e.saturating_mul(10) >= to.saturating_mul(UP_ABOVE_TENTHS))
            });
        let held = if headroom {
            now.saturating_sub(*self.headroom_since.get_or_insert(now))
        } else {
            self.headroom_since = None;
            0
        };
        let inputs = Inputs {
            cap: self.cap,
            ceiling: self.ceiling,
            source_bps: self.source_bps,
            current_bps: current,
            estimate_bps: estimate,
            stalls: self.stalls.len(),
            headroom_held_ms: held,
            settling,
        };
        let Decision::Change {
            cap,
            to_bps,
            reason,
        } = decide(&inputs)
        else {
            return None;
        };
        let change = Change {
            cap,
            from_bps: current.unwrap_or(0),
            to_bps,
            reason,
            estimate_bps: estimate.unwrap_or(0),
        };
        self.cap = cap;
        self.stalls.clear();
        self.headroom_since = None;
        self.stream_bps = None;
        self.quiet_until = now.saturating_add(SETTLE_MS);
        Some(change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1000;

    fn inputs() -> Inputs {
        Inputs {
            current_bps: Some(4_700_000),
            estimate_bps: Some(10_000_000),
            ..Inputs::default()
        }
    }

    fn change(cap: u64, to_bps: u64, reason: Reason) -> Decision {
        Decision::Change {
            cap,
            to_bps,
            reason,
        }
    }

    #[test]
    fn a_healthy_link_keeps_the_quality() {
        assert_eq!(decide(&inputs()), Decision::Keep);
    }

    #[test]
    fn two_stalls_step_down_to_the_next_rung_below_the_stream() {
        let i = Inputs {
            stalls: 2,
            estimate_bps: None,
            ..inputs()
        };
        assert_eq!(decide(&i), change(4_000_000, 4_000_000, Reason::Stalls));
    }

    #[test]
    fn one_stall_is_not_enough() {
        let i = Inputs {
            stalls: 1,
            ..inputs()
        };
        assert_eq!(decide(&i), Decision::Keep);
    }

    #[test]
    fn a_link_under_one_and_a_third_of_the_stream_steps_down_to_what_it_carries() {
        // 1.2 Mbit/s link, 4.7 Mbit/s stream: only the 1 Mbit/s rung fits.
        let i = Inputs {
            estimate_bps: Some(1_200_000),
            ..inputs()
        };
        assert_eq!(
            decide(&i),
            change(1_000_000, 1_000_000, Reason::LowEstimate)
        );
        // 3 Mbit/s: 2 Mbit/s * 1.3 fits, 4 does not.
        let i = Inputs {
            estimate_bps: Some(3_000_000),
            ..inputs()
        };
        assert_eq!(
            decide(&i),
            change(2_000_000, 2_000_000, Reason::LowEstimate)
        );
    }

    #[test]
    fn a_link_below_every_rung_goes_to_the_lowest() {
        let i = Inputs {
            estimate_bps: Some(500_000),
            ..inputs()
        };
        assert_eq!(
            decide(&i),
            change(1_000_000, 1_000_000, Reason::LowEstimate)
        );
    }

    #[test]
    fn the_lowest_rung_has_nowhere_to_go() {
        let i = Inputs {
            cap: 1_000_000,
            current_bps: Some(1_000_000),
            estimate_bps: Some(700_000),
            stalls: 5,
            ..inputs()
        };
        assert_eq!(decide(&i), Decision::Keep);
    }

    #[test]
    fn a_stream_below_the_lowest_rung_is_left_alone() {
        let i = Inputs {
            current_bps: Some(800_000),
            estimate_bps: Some(500_000),
            stalls: 3,
            ..inputs()
        };
        assert_eq!(decide(&i), Decision::Keep);
    }

    #[test]
    fn stalls_pick_the_next_rung_even_when_the_estimate_looks_fine() {
        let i = Inputs {
            stalls: 2,
            current_bps: Some(4_000_000),
            cap: 4_000_000,
            estimate_bps: Some(9_000_000),
            ..inputs()
        };
        assert_eq!(decide(&i), change(2_000_000, 2_000_000, Reason::Stalls));
    }

    #[test]
    fn nothing_is_decided_while_settling_or_without_a_stream_bitrate() {
        let settling = Inputs {
            stalls: 4,
            settling: true,
            ..inputs()
        };
        assert_eq!(decide(&settling), Decision::Keep);
        let blind = Inputs {
            stalls: 4,
            current_bps: None,
            ..inputs()
        };
        assert_eq!(decide(&blind), Decision::Keep);
    }

    #[test]
    fn sustained_headroom_steps_up_one_rung() {
        let i = Inputs {
            cap: 2_000_000,
            current_bps: Some(2_000_000),
            source_bps: Some(4_700_000),
            estimate_bps: Some(9_000_000),
            headroom_held_ms: 60 * MS,
            ..Inputs::default()
        };
        assert_eq!(decide(&i), change(4_000_000, 4_000_000, Reason::Headroom));
    }

    #[test]
    fn headroom_must_hold_for_a_minute() {
        let i = Inputs {
            cap: 2_000_000,
            current_bps: Some(2_000_000),
            source_bps: Some(4_700_000),
            estimate_bps: Some(9_000_000),
            headroom_held_ms: 59 * MS,
            ..Inputs::default()
        };
        assert_eq!(decide(&i), Decision::Keep);
    }

    #[test]
    fn the_last_step_up_lifts_the_cap_at_the_source_bitrate() {
        let i = Inputs {
            cap: 4_000_000,
            current_bps: Some(4_000_000),
            source_bps: Some(4_700_000),
            estimate_bps: Some(12_000_000),
            headroom_held_ms: 60 * MS,
            ..Inputs::default()
        };
        assert_eq!(decide(&i), change(0, 4_700_000, Reason::Headroom));
    }

    #[test]
    fn the_manual_ceiling_is_never_stepped_above() {
        assert_eq!(up_target(2_000_000, 2_000_000, Some(4_700_000)), None);
        assert_eq!(
            up_target(1_000_000, 2_000_000, Some(4_700_000)),
            Some((2_000_000, 2_000_000))
        );
        let i = Inputs {
            cap: 2_000_000,
            ceiling: 2_000_000,
            current_bps: Some(2_000_000),
            source_bps: Some(4_700_000),
            estimate_bps: Some(20_000_000),
            headroom_held_ms: 600 * MS,
            ..Inputs::default()
        };
        assert_eq!(decide(&i), Decision::Keep);
    }

    #[test]
    fn up_targets_walk_the_rungs_and_stop_at_the_top() {
        assert_eq!(up_target(0, 0, None), None);
        assert_eq!(up_target(1_000_000, 0, None), Some((2_000_000, 2_000_000)));
        assert_eq!(up_target(8_000_000, 0, None), None);
        // A source below the next rung is reached by lifting the cap.
        assert_eq!(
            up_target(2_000_000, 0, Some(3_000_000)),
            Some((0, 3_000_000))
        );
    }

    fn watch() -> Watch {
        Watch::new(0, 0, 0)
    }

    /// Three 6 s fragments of `stream_bps` that downloaded at `link_bps`.
    fn downloads(w: &mut Watch, stream_bps: u64, link_bps: u64, count: u32) {
        for _ in 0..count {
            let bytes = stream_bps * 6 / 8;
            let load_ms = bytes * 8 * 1000 / link_bps;
            w.fragment(bytes, load_ms, 6_000);
        }
    }

    #[test]
    fn a_watch_steps_down_on_repeated_stalls_after_settling() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.stalled(20 * MS);
        assert_eq!(w.evaluate(20 * MS), None);
        w.stalled(25 * MS);
        let c = w.evaluate(25 * MS);
        assert_eq!(
            c.map(|c| (c.cap, c.reason)),
            Some((4_000_000, Reason::Stalls))
        );
        assert_eq!(w.cap(), 4_000_000);
    }

    #[test]
    fn stalls_while_settling_are_ignored() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.stalled(2 * MS);
        w.stalled(8 * MS);
        assert_eq!(w.evaluate(9 * MS), None);
    }

    #[test]
    fn one_stall_reported_twice_counts_once() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.stalled(20 * MS);
        w.stalled(20 * MS + 500);
        assert_eq!(w.evaluate(21 * MS), None);
    }

    #[test]
    fn a_seeks_own_waiting_is_not_a_stall() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.stalled(20 * MS);
        w.seeking(25 * MS);
        w.stalled(25 * MS);
        w.seeked();
        assert_eq!(w.evaluate(26 * MS), None);
    }

    #[test]
    fn a_slow_seek_is_not_a_stall() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.seeking(20 * MS);
        w.stalled(20 * MS);
        w.seeked();
        assert_eq!(w.evaluate(24 * MS), None);
        // One real stall after the seek is still one: the seek's waiting did not count.
        w.stalled(25 * MS);
        assert_eq!(w.evaluate(25 * MS), None);
    }

    #[test]
    fn two_slow_seeks_do_not_step_down() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.seeking(20 * MS);
        w.stalled(20 * MS);
        w.seeked();
        w.seeking(30 * MS);
        w.stalled(30 * MS);
        w.seeked();
        assert_eq!(w.evaluate(35 * MS), None);
    }

    #[test]
    fn a_stall_after_a_seek_still_counts() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.seeking(20 * MS);
        w.seeked();
        w.stalled(25 * MS);
        w.stalled(30 * MS);
        assert!(w.evaluate(30 * MS).is_some());
    }

    #[test]
    fn old_stalls_expire() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.stalled(20 * MS);
        w.stalled(60 * MS);
        assert_eq!(w.evaluate(61 * MS), None);
    }

    #[test]
    fn a_slow_link_steps_down_without_a_stall() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 1_200_000, 3);
        let c = w.evaluate(20 * MS);
        let c = c.map(|c| (c.cap, c.from_bps, c.reason));
        assert_eq!(c, Some((1_000_000, 4_700_000, Reason::LowEstimate)));
    }

    #[test]
    fn the_estimate_is_not_believed_before_three_downloads() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 1_200_000, 2);
        assert_eq!(w.evaluate(20 * MS), None);
    }

    #[test]
    fn a_cache_hit_does_not_make_the_link_look_fast() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        let before = (w.estimate_bps, w.samples);
        w.fragment(100_000, 1, 6_000);
        assert_eq!((w.estimate_bps, w.samples), before);
    }

    #[test]
    fn no_single_download_is_believed_above_200_mbit() {
        let mut w = watch();
        w.fragment(10_000_000, 50, 6_000);
        assert_eq!(w.estimate_bps, Some(200_000_000));
    }

    #[test]
    fn a_watch_steps_up_after_a_minute_of_headroom_and_not_before() {
        let mut w = watch();
        // Learn the source, then step down on a slow link.
        downloads(&mut w, 4_700_000, 1_200_000, 3);
        assert!(w.evaluate(20 * MS).is_some());
        // The link recovers; the stream is now the 1 Mbit/s transcode.
        downloads(&mut w, 1_000_000, 12_000_000, 8);
        assert_eq!(w.evaluate(40 * MS), None);
        assert_eq!(w.evaluate(99 * MS), None);
        let c = w.evaluate(101 * MS);
        assert_eq!(
            c.map(|c| (c.cap, c.reason)),
            Some((2_000_000, Reason::Headroom))
        );
    }

    #[test]
    fn a_stall_restarts_the_headroom_clock() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 1_200_000, 3);
        assert!(w.evaluate(20 * MS).is_some());
        downloads(&mut w, 1_000_000, 12_000_000, 8);
        assert_eq!(w.evaluate(40 * MS), None);
        w.stalled(80 * MS);
        assert_eq!(w.evaluate(81 * MS), None);
        assert_eq!(w.evaluate(101 * MS), None);
        assert!(w.evaluate(141 * MS).is_some());
    }

    #[test]
    fn a_restarted_player_settles_before_stalls_count() {
        let mut w = watch();
        downloads(&mut w, 4_700_000, 20_000_000, 3);
        w.restarted(30 * MS);
        w.stalled(31 * MS);
        w.stalled(36 * MS);
        assert_eq!(w.evaluate(37 * MS), None);
        w.stalled(46 * MS);
        w.stalled(50 * MS);
        assert!(w.evaluate(51 * MS).is_some());
    }

    #[test]
    fn the_manual_maximum_and_the_automatic_cap_combine_to_the_lower() {
        assert_eq!(effective_cap(0, 0), 0);
        assert_eq!(effective_cap(4_000_000, 0), 4_000_000);
        assert_eq!(effective_cap(0, 2_000_000), 2_000_000);
        assert_eq!(effective_cap(2_000_000, 4_000_000), 2_000_000);
        assert_eq!(effective_cap(4_000_000, 1_000_000), 1_000_000);
    }

    #[test]
    fn a_stored_rung_is_read_only_when_it_is_one() {
        assert_eq!(parse_rung("2000000"), 2_000_000);
        assert_eq!(parse_rung(" 1000000 "), 1_000_000);
        assert_eq!(parse_rung("0"), 0);
        assert_eq!(parse_rung("3000000"), 0);
        assert_eq!(parse_rung("fast"), 0);
        assert_eq!(parse_rung(""), 0);
    }

    #[test]
    fn a_remembered_cap_starts_under_the_ceiling() {
        assert_eq!(Watch::new(0, 2_000_000, 0).cap(), 2_000_000);
        assert_eq!(Watch::new(1_000_000, 2_000_000, 0).cap(), 1_000_000);
        assert_eq!(Watch::new(2_000_000, 0, 0).cap(), 0);
    }
}
