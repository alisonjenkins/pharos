//! Jumping a library grid by first letter (B312/V252): which letter a tile
//! is under, where the next or previous one starts, and how far a wheel must
//! turn to step one. Pure, so the rules run under host tests; the rail, the
//! wheel listener and the scrolling live in `browse` and the platform module.

/// The letters the rail offers, in order: `#` for names that do not start
/// A to Z, then A to Z.
pub const LETTERS: [char; 27] = [
    '#', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R',
    'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

/// How far a Shift+wheel turns before it steps one letter, in CSS pixels. A
/// mouse notch is about 100; a trackpad sends many small deltas, which add
/// up to one step rather than one each.
pub const WHEEL_STEP: f64 = 80.0;

/// Where `letter` starts among `starts` (each letter and the offset of its
/// first tile, in listing order), if the listing holds it.
pub fn start_of(starts: &[(char, u32)], letter: char) -> Option<u32> {
    starts.iter().find(|&&(l, _)| l == letter).map(|&(_, s)| s)
}

/// The letter the tile at `at` is under: the one that starts last at or
/// before it.
pub fn letter_at(starts: &[(char, u32)], at: u32) -> Option<char> {
    starts
        .iter()
        .filter(|&&(_, s)| s <= at)
        .max_by_key(|&&(_, s)| s)
        .map(|&(l, _)| l)
}

/// Where a step from the tile at `at` lands: forward, the first letter that
/// starts after it; back, the last that starts before it (the start of its
/// own letter when it sits inside one). `None` at either end.
pub fn step(starts: &[(char, u32)], at: u32, forward: bool) -> Option<u32> {
    let mut all: Vec<u32> = starts.iter().map(|&(_, s)| s).collect();
    all.sort_unstable();
    if forward {
        all.into_iter().find(|&s| s > at)
    } else {
        all.into_iter().rev().find(|&s| s < at)
    }
}

/// Adds up Shift+wheel turns into whole letter steps.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Wheel {
    turned: f64,
}

impl Wheel {
    /// Adds one wheel event's `delta` and answers how many steps it makes:
    /// positive forward, negative back. Turning the other way drops what
    /// had built up the first way.
    pub fn feed(&mut self, delta: f64) -> i32 {
        if delta == 0.0 || !delta.is_finite() {
            return 0;
        }
        if self.turned != 0.0 && self.turned.signum() != delta.signum() {
            self.turned = 0.0;
        }
        self.turned += delta;
        let steps = (self.turned / WHEEL_STEP).trunc();
        self.turned -= steps * WHEEL_STEP;
        // A step is at most a few dozen; the cast cannot overflow.
        steps as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTS: [(char, u32); 4] = [('#', 0), ('A', 2), ('C', 10), ('Z', 40)];

    #[test]
    fn a_tile_is_under_the_last_letter_starting_at_or_before_it() {
        assert_eq!(letter_at(&STARTS, 0), Some('#'));
        assert_eq!(letter_at(&STARTS, 2), Some('A'));
        assert_eq!(letter_at(&STARTS, 9), Some('A'));
        assert_eq!(letter_at(&STARTS, 99), Some('Z'));
        assert_eq!(letter_at(&[], 3), None);
    }

    #[test]
    fn a_step_forward_skips_letters_the_listing_lacks() {
        assert_eq!(step(&STARTS, 2, true), Some(10));
        assert_eq!(step(&STARTS, 5, true), Some(10));
        assert_eq!(step(&STARTS, 40, true), None);
    }

    #[test]
    fn a_step_back_goes_to_the_start_of_the_letter_first() {
        assert_eq!(step(&STARTS, 12, false), Some(10));
        assert_eq!(step(&STARTS, 10, false), Some(2));
        assert_eq!(step(&STARTS, 0, false), None);
    }

    #[test]
    fn a_descending_listing_steps_in_its_own_order() {
        let starts = [('Z', 0), ('C', 30), ('A', 38)];
        assert_eq!(step(&starts, 0, true), Some(30));
        assert_eq!(letter_at(&starts, 31), Some('C'));
    }

    #[test]
    fn the_rail_finds_only_letters_the_listing_holds() {
        assert_eq!(start_of(&STARTS, 'C'), Some(10));
        assert_eq!(start_of(&STARTS, 'B'), None);
    }

    #[test]
    fn a_mouse_notch_steps_once_and_small_turns_add_up() {
        let mut w = Wheel::default();
        assert_eq!(w.feed(100.0), 1);
        let mut w = Wheel::default();
        assert_eq!((0..7).map(|_| w.feed(12.0)).sum::<i32>(), 1);
        assert_eq!(w.feed(-100.0), -1);
        assert_eq!(w.feed(0.0), 0);
    }

    #[test]
    fn turning_back_drops_the_turn_built_up_forward() {
        let mut w = Wheel::default();
        assert_eq!(w.feed(70.0), 0);
        assert_eq!(w.feed(-70.0), 0);
        assert_eq!(w.feed(70.0), 0);
    }
}
