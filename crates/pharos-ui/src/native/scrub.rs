//! Remote-control scrubbing for the TV player (B907, V855): left and right
//! move a preview position with its trickplay thumbnail instead of seeking on
//! every press. A tap moves 10 s; a held key moves faster the longer it is
//! held. The preview is committed by OK or play, or once the keys have been
//! still for [`SETTLE_MS`]; Back drops it and playback carries on unmoved.
//!
//! Pure state over a millisecond clock, so the TV builds (WebView and Blitz)
//! and the tests share one rule.

/// How far one tap moves the preview.
pub const TAP_STEP_MS: u64 = 10_000;
/// Keys still this long commit the preview.
pub const SETTLE_MS: u32 = 900;
/// A held key never crosses the whole video in less than this, except a
/// video under 130 s: from its start, the opening 10 s tap plus the rest at
/// the 20x floor already cross it sooner.
const MIN_CROSSING_MS: f64 = 6_000.0;

/// A preview under way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scrub {
    /// Where playback was when the preview began; Back returns here.
    pub origin_ms: u64,
    /// Where the preview is.
    pub target_ms: u64,
    pub forward: bool,
    /// When the first press of this preview arrived, for the log.
    pub started_at: f64,
    /// When the current hold began (the press that started it).
    pub hold_start: f64,
    /// When the last press arrived; the settle timer compares against it.
    pub last_press: f64,
    /// Presses in this preview, for the commit log; a pointer drag's moves.
    pub presses: u32,
    /// What drives the preview.
    pub input: Input,
}

/// What drives a preview: the remote's left and right, or a pointer
/// dragging the seek bar (T038-301).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Remote,
    Mouse,
    Touch,
    Pen,
}

impl Input {
    /// A pointer event's `pointerType`; an unknown one counts as a mouse.
    pub fn of_pointer(kind: &str) -> Self {
        match kind {
            "touch" => Self::Touch,
            "pen" => Self::Pen,
            _ => Self::Mouse,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Mouse => "mouse",
            Self::Touch => "touch",
            Self::Pen => "pen",
        }
    }
}

/// The preview after a pointer on the seek bar moved to `target_ms`: the
/// first call (no `prev`) is the press, which starts the preview from
/// playback's `position_ms`. Nothing seeks until the pointer lets go.
pub fn drag(
    prev: Option<Scrub>,
    input: Input,
    target_ms: u64,
    position_ms: u64,
    now: f64,
) -> Scrub {
    let prev = prev.filter(|s| s.input == input);
    Scrub {
        origin_ms: prev.map_or(position_ms, |s| s.origin_ms),
        target_ms,
        forward: target_ms >= prev.map_or(position_ms, |s| s.origin_ms),
        started_at: prev.map_or(now, |s| s.started_at),
        hold_start: prev.map_or(now, |s| s.hold_start),
        last_press: now,
        presses: prev.map_or(0, |s| s.presses) + 1,
        input,
    }
}

/// Video milliseconds a held key moves per real millisecond, `held_ms` into
/// the hold: 20x for the first second, then 60x, 180x and 480x, as a held
/// Netflix scrub speeds up.
pub fn hold_speed(held_ms: f64) -> f64 {
    match held_ms {
        h if h < 1_000.0 => 20.0,
        h if h < 2_500.0 => 60.0,
        h if h < 4_500.0 => 180.0,
        _ => 480.0,
    }
}

/// The preview after one press of left (`forward == false`) or right.
/// `repeat` is the platform's key-repeat flag, the only sign of a hold: two
/// quick taps are two taps, 10 s each. `position_ms` is playback's
/// position, used when no preview is under way; `duration_ms` 0 means
/// unknown, which leaves the end unbounded.
pub fn press(
    prev: Option<Scrub>,
    forward: bool,
    repeat: bool,
    position_ms: u64,
    duration_ms: u64,
    now: f64,
) -> Scrub {
    let holding = prev.filter(|s| repeat && s.forward == forward);
    let base = prev.map_or(position_ms, |s| s.target_ms);
    let (hold_start, by) = match holding {
        Some(s) => {
            let mut speed = hold_speed(now - s.hold_start);
            if duration_ms > 0 {
                // What the hold has left to cross once the opening tap has
                // moved 10 s, in no less than MIN_CROSSING_MS.
                let rest = duration_ms.saturating_sub(TAP_STEP_MS) as f64;
                speed = speed.min((rest / MIN_CROSSING_MS).max(hold_speed(0.0)));
            }
            let dt = (now - s.last_press).max(0.0);
            // Floored, so a press never moves more than the speed allows:
            // rounding half up adds a little each repeat, and over a fast
            // remote's hundreds of repeats that crosses short of the floor.
            (s.hold_start, (speed * dt).floor() as u64)
        }
        None => (now, TAP_STEP_MS),
    };
    let target = if forward {
        let to = base.saturating_add(by);
        if duration_ms > 0 {
            to.min(duration_ms)
        } else {
            to
        }
    } else {
        base.saturating_sub(by)
    };
    Scrub {
        origin_ms: prev.map_or(position_ms, |s| s.origin_ms),
        started_at: prev.map_or(now, |s| s.started_at),
        target_ms: target,
        forward,
        hold_start,
        last_press: now,
        presses: prev.map_or(0, |s| s.presses) + 1,
        input: Input::Remote,
    }
}

/// Why a preview ended, for the log line each end writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The keys were still for [`SETTLE_MS`].
    Settled,
    /// OK or play.
    Confirmed,
    /// Back: no seek.
    Cancelled,
    /// The pointer dragging the seek bar let go.
    Released,
}

impl End {
    pub fn label(self) -> &'static str {
        match self {
            End::Settled => "settled",
            End::Confirmed => "confirmed",
            End::Cancelled => "cancelled",
            End::Released => "released",
        }
    }
}

/// The log line a preview's end writes, so a scrub can be followed in the
/// TV's logs: where it started and ended, how, after how many presses, and
/// what drove it (`input=remote` for the D-pad).
pub fn end_line(s: &Scrub, end: End, now: f64) -> String {
    let steps = if s.input == Input::Remote {
        "presses"
    } else {
        "moves"
    };
    format!(
        "pharos: scrub {} input={} from {} ms to {} ms after {} {steps} in {} ms",
        end.label(),
        s.input.label(),
        s.origin_ms,
        s.target_ms,
        s.presses,
        (now - s.started_at).max(0.0).round() as u64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;

    #[test]
    fn a_tap_moves_ten_seconds_from_playback() {
        let s = press(None, true, false, 60_000, HOUR, 0.0);
        assert_eq!((s.origin_ms, s.target_ms, s.presses), (60_000, 70_000, 1));
        let s = press(Some(s), true, false, 99_999, HOUR, 1_000.0);
        assert_eq!(
            s.target_ms, 80_000,
            "a second tap moves the preview, not playback"
        );
        let s = press(Some(s), false, false, 0, HOUR, 2_000.0);
        assert_eq!((s.origin_ms, s.target_ms), (60_000, 70_000));
    }

    #[test]
    fn a_held_key_speeds_up() {
        // A remote repeating every 50 ms for six seconds.
        let mut s = press(None, true, false, 0, 4 * HOUR, 0.0);
        let mut moved = Vec::new();
        let mut last = s.target_ms;
        for tick in 1..=120u32 {
            s = press(Some(s), true, true, 0, 4 * HOUR, f64::from(tick) * 50.0);
            if tick % 20 == 0 {
                moved.push(s.target_ms - last);
                last = s.target_ms;
            }
        }
        assert!(
            moved.windows(2).all(|w| w[1] >= w[0]) && moved[5] > 10 * moved[0],
            "each second of hold moves at least as far as the last: {moved:?}"
        );
        assert!(
            moved[0] < 30_000,
            "the first second stays fine-grained: {moved:?}"
        );
    }

    #[test]
    fn a_held_key_is_capped_so_the_video_is_not_crossed_in_a_blink() {
        // 30 min: the cap is (1_800_000 - 10_000) / 6_000 = 298.3x, below a
        // long hold's 480x.
        let duration = 1_800_000;
        let mut s = press(None, true, false, 0, duration, 0.0);
        let mut last = s.target_ms;
        for i in 1..=120 {
            last = s.target_ms;
            s = press(Some(s), true, true, 0, duration, f64::from(i) * 50.0);
        }
        assert!(s.target_ms < duration, "the clamp must not hide the cap");
        assert_eq!(s.target_ms - last, 14_916, "298.3x over 50 ms, not 480x");
    }

    /// Real milliseconds a hold from 0, repeating every `every` ms, takes to
    /// reach the end of a video `duration` long.
    fn crossing_ms(duration: u64, every: f64) -> f64 {
        let mut s = press(None, true, false, 0, duration, 0.0);
        let mut now = 0.0;
        while s.target_ms < duration {
            now += every;
            s = press(Some(s), true, true, 0, duration, now);
        }
        now
    }

    #[test]
    fn the_crossing_floor_holds_from_130_seconds_up() {
        for duration in [130_000, 130_300, 300_000, 1_800_000, HOUR, 4 * HOUR] {
            for every in [10.0, 33.0, 50.0] {
                let t = crossing_ms(duration, every);
                assert!(
                    t >= MIN_CROSSING_MS,
                    "{duration} ms crossed in {t} ms at {every} ms repeats"
                );
            }
        }
        let t = crossing_ms(120_000, 50.0);
        assert!(
            t < MIN_CROSSING_MS,
            "under 130 s the tap and the 20x floor win: {t} ms"
        );
    }

    #[test]
    fn the_preview_stays_inside_the_video() {
        let s = press(None, false, false, 4_000, HOUR, 0.0);
        assert_eq!(s.target_ms, 0);
        let s = press(None, true, false, HOUR - 3_000, HOUR, 0.0);
        assert_eq!(s.target_ms, HOUR);
        let s = press(None, true, false, HOUR, 0, 0.0);
        assert_eq!(
            s.target_ms,
            HOUR + TAP_STEP_MS,
            "unknown length leaves the end open"
        );
    }

    #[test]
    fn turning_round_starts_a_new_hold() {
        let mut s = press(None, true, false, 0, HOUR, 0.0);
        for i in 1..=100 {
            s = press(Some(s), true, true, 0, HOUR, f64::from(i) * 50.0);
        }
        let back = press(Some(s), false, true, 0, HOUR, 5_050.0);
        assert_eq!(back.target_ms, s.target_ms - TAP_STEP_MS);
        assert_eq!(back.hold_start, 5_050.0);
    }

    #[test]
    fn quick_taps_are_taps_and_a_repeat_is_a_hold() {
        let s = press(None, true, false, 0, HOUR, 0.0);
        let tap = press(Some(s), true, false, 0, HOUR, 100.0);
        assert_eq!(
            tap.target_ms,
            2 * TAP_STEP_MS,
            "a quick second tap is 10 s more"
        );
        let held = press(Some(s), true, true, 0, HOUR, 100.0);
        assert_eq!(
            held.target_ms,
            TAP_STEP_MS + 2_000,
            "a repeat moves 20x over 100 ms"
        );
    }

    #[test]
    fn the_end_line_names_how_and_where() {
        let s = press(None, true, false, 60_000, HOUR, 0.0);
        let s = press(Some(s), true, false, 60_000, HOUR, 1_000.0);
        let line = end_line(&s, End::Cancelled, 1_900.0);
        assert_eq!(
            line, "pharos: scrub cancelled input=remote from 60000 ms to 80000 ms after 2 presses in 1900 ms",
            "the time is the whole preview's, not the last hold's"
        );
        let labels = [End::Settled, End::Confirmed, End::Cancelled, End::Released].map(End::label);
        assert!(labels
            .iter()
            .enumerate()
            .all(|(i, a)| labels[i + 1..].iter().all(|b| a != b)));
    }

    #[test]
    fn a_drag_previews_until_it_lets_go_and_logs_its_input() {
        let s = drag(None, Input::Touch, 90_000, 60_000, 0.0);
        assert_eq!((s.origin_ms, s.target_ms, s.presses), (60_000, 90_000, 1));
        let s = drag(Some(s), Input::Touch, 30_000, 75_000, 400.0);
        assert_eq!(
            (s.origin_ms, s.target_ms, s.presses, s.forward),
            (60_000, 30_000, 2, false),
            "the origin is where playback was at the press"
        );
        assert_eq!(
            end_line(&s, End::Released, 650.0),
            "pharos: scrub released input=touch from 60000 ms to 30000 ms after 2 moves in 650 ms"
        );
        let remote = press(None, true, false, 0, HOUR, 0.0);
        let s = drag(Some(remote), Input::Mouse, 5_000, 1_000, 10.0);
        assert_eq!(
            (s.origin_ms, s.presses),
            (1_000, 1),
            "a drag never continues a remote preview"
        );
        let kinds = ["touch", "pen", "mouse", ""].map(|k| Input::of_pointer(k).label());
        assert_eq!(kinds, ["touch", "pen", "mouse", "mouse"]);
    }
}
