//! Rail gestures as plain arithmetic: how far a sideways wheel turn moves a
//! rail. The listener that feeds it lives in the platform module.

/// How much further a sideways wheel turn moves a rail than the browser
/// would: one notch is ~100 px, under one poster, so crossing a shelf took
/// a dozen notches (B315).
pub const WHEEL_BOOST: f64 = 3.0;

/// The smallest pixel turn taken as a Shift+wheel notch. A trackpad sends
/// many small turns and already moves a rail under the finger; boosting
/// those would fling it.
const NOTCH_PX: f64 = 40.0;

/// The smallest pixel turn taken as a tilt-wheel notch with no Shift held.
/// Higher, since a fast trackpad swipe also sends purely sideways turns;
/// tilt wheels send ~100 px or more.
const TILT_NOTCH_PX: f64 = 100.0;

/// How long after one boosted turn the next adds to where the first was
/// heading rather than to where the rail is mid-glide, in ms. Without it a
/// quick spin loses most of each notch to the glide still in flight.
const GLIDE_MS: f64 = 300.0;

/// How far a wheel turn of `dx`, `dy` (in pixels) should move a rail, or
/// `None` to leave it to the browser. Only a notch-sized sideways turn is
/// boosted: Shift+wheel (sent as `dy` by some browsers, `dx` by others) or
/// a tilt wheel; a turn the browser reports in `lines` or pages is always a
/// wheel. A plain vertical wheel scrolls the page (B295), and a trackpad's
/// small or diagonal turns are already right.
pub fn wheel_step(dx: f64, dy: f64, shift: bool, lines: bool) -> Option<f64> {
    let turn = if shift && dx == 0.0 {
        dy
    } else if dy == 0.0 {
        dx
    } else {
        return None;
    };
    let notch = if shift { NOTCH_PX } else { TILT_NOTCH_PX };
    (turn != 0.0 && (lines || turn.abs() >= notch)).then_some(turn * WHEEL_BOOST)
}

/// Where a boosted turn is taking a rail.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glide {
    pub to: f64,
    /// The event time it was set, in ms.
    pub at: f64,
}

/// Where a rail scrolled to `left` of `max` should glide for a `step` at
/// time `now`, given the last glide. `None` when it is already at the end
/// it is turned towards, so the turn goes on to the page.
pub fn glide(last: Option<Glide>, now: f64, left: f64, max: f64, step: f64) -> Option<Glide> {
    let at_end = (step < 0.0 && left <= 0.0) || (step > 0.0 && left >= max - 1.0);
    if max <= 0.0 || at_end {
        return None;
    }
    let from = last
        .filter(|g| now - g.at < GLIDE_MS && (g.to - left).signum() == step.signum())
        .map_or(left, |g| g.to);
    Some(Glide {
        to: (from + step).clamp(0.0, max),
        at: now,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_shifted_or_tilted_notch_moves_further_than_its_turn() {
        assert_eq!(wheel_step(0.0, 100.0, true, false), Some(300.0));
        assert_eq!(wheel_step(-53.0, 0.0, true, false), Some(-159.0));
        assert_eq!(wheel_step(120.0, 0.0, false, false), Some(360.0));
    }

    #[test]
    fn a_notch_in_lines_is_a_wheel_however_short() {
        assert_eq!(wheel_step(48.0, 0.0, false, true), Some(144.0));
        assert_eq!(wheel_step(0.0, 0.0, false, true), None);
    }

    #[test]
    fn a_plain_wheel_or_trackpad_swipe_is_left_to_the_browser() {
        assert_eq!(wheel_step(0.0, 100.0, false, false), None);
        assert_eq!(wheel_step(12.0, 0.0, false, false), None);
        // A fast swipe can be purely sideways and well past a Shift notch.
        assert_eq!(wheel_step(80.0, 0.0, false, false), None);
        assert_eq!(wheel_step(60.0, 5.0, false, false), None);
    }

    #[test]
    fn quick_notches_add_up_from_where_the_glide_is_heading() {
        let first = glide(None, 0.0, 100.0, 2000.0, 300.0).unwrap();
        assert_eq!(first.to, 400.0);
        let second = glide(Some(first), 50.0, 150.0, 2000.0, 300.0).unwrap();
        assert_eq!(second.to, 700.0);
        let later = glide(Some(second), 1000.0, 700.0, 2000.0, 300.0).unwrap();
        assert_eq!(later.to, 1000.0);
    }

    #[test]
    fn a_turn_back_starts_from_where_the_rail_is() {
        let first = glide(None, 0.0, 100.0, 2000.0, 300.0).unwrap();
        let back = glide(Some(first), 50.0, 150.0, 2000.0, -300.0).unwrap();
        assert_eq!(back.to, 0.0);
    }

    #[test]
    fn a_glide_stops_at_the_ends_and_an_end_turn_goes_to_the_page() {
        assert_eq!(glide(None, 0.0, 1900.0, 2000.0, 300.0).unwrap().to, 2000.0);
        assert_eq!(glide(None, 0.0, 2000.0, 2000.0, 300.0), None);
        assert_eq!(glide(None, 0.0, 0.0, 2000.0, -300.0), None);
        assert_eq!(glide(None, 0.0, 0.0, 0.0, 300.0), None);
    }
}
