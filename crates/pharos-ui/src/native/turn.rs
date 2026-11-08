//! The episode turn (V790): a cube turn from the episode on screen to the
//! next, or onto the episode being opened, that follows the load. The click
//! turns the new face most of the way in at once; each loading step that
//! completes turns it a little further; the first frame lands it flat. Pure
//! rules here, so they test on the host; the layer itself is `turn_web.rs`.

use std::cell::Cell;

/// Where the viewer's choice is kept; absent or unknown is [`EpisodeTurn::Turn`].
pub const EPISODE_TURN_KEY: &str = "pharos.player.turn";

/// Whether opening an episode, or moving to the next, turns like a cube.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeTurn {
    #[default]
    Turn,
    Off,
}

impl EpisodeTurn {
    /// Every choice, in the Playback page's order.
    pub const ALL: [EpisodeTurn; 2] = [EpisodeTurn::Turn, EpisodeTurn::Off];

    /// The choice a stored [`EPISODE_TURN_KEY`] value holds.
    pub fn parse(stored: &str) -> EpisodeTurn {
        match stored.trim() {
            "off" => EpisodeTurn::Off,
            _ => EpisodeTurn::Turn,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EpisodeTurn::Turn => "turn",
            EpisodeTurn::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EpisodeTurn::Turn => "Turn",
            EpisodeTurn::Off => "Off",
        }
    }
}

/// Whether a turn runs: chosen, and the viewer has not asked for less motion.
pub fn wanted(choice: EpisodeTurn, reduced_motion: bool) -> bool {
    choice == EpisodeTurn::Turn && !reduced_motion
}

/// How far the load has got, as the layer hears it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// The click: nothing has loaded yet.
    Clicked,
    /// The playback is negotiated and the player mounted.
    Negotiated,
    /// The stream's metadata is read (`loadedmetadata`).
    Metadata,
    /// Media is buffered this many ms ahead of the start point.
    Buffered(u32),
    /// The first frame can show (`loadeddata`): the turn lands.
    FirstFrame,
}

/// The angle the face starts at, edge-on like a cube's next side.
pub const START_DEG: f64 = 90.0;

/// The angle the click turns the face to. Most of the turn happens at once,
/// so the press shows; a slow load then waits at a modest angle rather than
/// a steep one that reads as broken.
const CLICKED_DEG: f64 = 30.0;
const NEGOTIATED_DEG: f64 = 22.0;
const METADATA_DEG: f64 = 15.0;
/// Buffering turns the face from [`METADATA_DEG`] down to this as
/// [`BUFFER_FULL_MS`] of media comes in.
const BUFFERED_MIN_DEG: f64 = 8.0;
/// About what a player buffers before its first frame; only a scale for the
/// angle, never shown as a number.
const BUFFER_FULL_MS: u32 = 3_000;

/// How long the click's first move takes.
pub const FIRST_MOVE_MS: u32 = 420;
/// How long each later loading step's move takes.
pub const STEP_MS: u32 = 380;
/// How long the landing, and the old episode's turn away, take: long enough
/// to notice (Alison found 220-300 ms too quick to see).
pub const LAND_MS: u32 = 650;
/// A turn with no first frame by then is given up, so it never hides a
/// player that has stalled or failed.
pub const GIVE_UP_MS: u32 = 30_000;
/// No caption for a load that is quick anyway.
const CAPTION_AFTER_MS: u32 = 2_500;
/// No step for this long reads as a stall.
const STALLED_AFTER_MS: u32 = 6_000;

/// The face's angle, in degrees, once `step` has been heard. Never rises
/// again: [`Turn::hear`] keeps the smallest.
pub fn angle(step: Step) -> f64 {
    match step {
        Step::Clicked => CLICKED_DEG,
        Step::Negotiated => NEGOTIATED_DEG,
        Step::Metadata => METADATA_DEG,
        Step::Buffered(ms) => {
            let filled = f64::from(ms.min(BUFFER_FULL_MS)) / f64::from(BUFFER_FULL_MS);
            METADATA_DEG - (METADATA_DEG - BUFFERED_MIN_DEG) * filled
        }
        Step::FirstFrame => 0.0,
    }
}

/// The caption under a slow load, if one is due: what it is waiting on, or
/// that the server has gone quiet.
pub fn caption(step: Step, since_begin_ms: u32, since_step_ms: u32) -> Option<&'static str> {
    if since_begin_ms < CAPTION_AFTER_MS || step == Step::FirstFrame {
        return None;
    }
    if since_step_ms >= STALLED_AFTER_MS {
        return Some("Waiting for the server…");
    }
    Some(match step {
        Step::Clicked => "Starting playback…",
        Step::Negotiated => "Opening the stream…",
        Step::Metadata | Step::Buffered(_) => "Buffering…",
        Step::FirstFrame => return None,
    })
}

/// Which way the cube turns: the next episode comes in from the right, the
/// previous one from the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Back,
}

impl Direction {
    pub fn sign(self) -> f64 {
        match self {
            Direction::Forward => 1.0,
            Direction::Back => -1.0,
        }
    }
}

/// The CSS transform of a cube face turned `deg` about the cube's centre,
/// half the viewport's width behind the screen.
pub fn face_transform(deg: f64) -> String {
    format!("translateZ(-50vw) rotateY({deg:.2}deg) translateZ(50vw)")
}

/// One turn's progress: the furthest step heard and when.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Turn {
    pub step: Step,
    pub begun_ms: f64,
    pub stepped_ms: f64,
}

impl Turn {
    pub fn new(now_ms: f64) -> Turn {
        Turn {
            step: Step::Clicked,
            begun_ms: now_ms,
            stepped_ms: now_ms,
        }
    }

    /// Take `step` if it moves the face further in; returns the angle to
    /// turn to, or `None` when it changes nothing (a late or repeated event).
    pub fn hear(&mut self, step: Step, now_ms: f64) -> Option<f64> {
        let to = angle(step);
        if to >= angle(self.step) {
            return None;
        }
        self.step = step;
        self.stepped_ms = now_ms;
        Some(to)
    }

    pub fn caption(&self, now_ms: f64) -> Option<&'static str> {
        caption(
            self.step,
            elapsed(self.begun_ms, now_ms),
            elapsed(self.stepped_ms, now_ms),
        )
    }

    pub fn given_up(&self, now_ms: f64) -> bool {
        elapsed(self.begun_ms, now_ms) >= GIVE_UP_MS
    }
}

fn elapsed(from_ms: f64, now_ms: f64) -> u32 {
    let ms = (now_ms - from_ms).max(0.0).min(f64::from(u32::MAX));
    // In range and non-negative after the clamp above.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = ms as u32;
    ms
}

thread_local! {
    /// Set by Previous just before it navigates, so that turn runs backwards.
    static GOING_BACK: Cell<bool> = const { Cell::new(false) };
}

/// The next navigation is a step back.
pub fn going_back() {
    GOING_BACK.set(true);
}

/// The direction of the navigation now happening; spends a [`going_back`].
pub fn take_direction() -> Direction {
    if GOING_BACK.replace(false) {
        Direction::Back
    } else {
        Direction::Forward
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_or_missing_choice_turns() {
        assert_eq!(EpisodeTurn::parse(""), EpisodeTurn::Turn);
        assert_eq!(EpisodeTurn::parse("sideways"), EpisodeTurn::Turn);
        assert_eq!(EpisodeTurn::parse(" off "), EpisodeTurn::Off);
        for c in EpisodeTurn::ALL {
            assert_eq!(EpisodeTurn::parse(c.as_str()), c);
        }
    }

    #[test]
    fn reduced_motion_or_off_means_no_turn() {
        assert!(wanted(EpisodeTurn::Turn, false));
        assert!(!wanted(EpisodeTurn::Turn, true));
        assert!(!wanted(EpisodeTurn::Off, false));
    }

    #[test]
    fn each_step_turns_the_face_further_in_and_the_first_frame_lands_it() {
        let steps = [
            Step::Clicked,
            Step::Negotiated,
            Step::Metadata,
            Step::Buffered(1_000),
            Step::Buffered(3_000),
            Step::FirstFrame,
        ];
        let angles: Vec<f64> = steps.iter().map(|&s| angle(s)).collect();
        assert!(angles.windows(2).all(|w| w[0] > w[1]), "{angles:?}");
        assert!(angle(Step::Clicked) < START_DEG);
        assert_eq!(angle(Step::FirstFrame), 0.0);
        // A slow load waits at a modest angle, never a steep one.
        assert!(angle(Step::Clicked) <= 30.0);
    }

    #[test]
    fn buffering_past_the_scale_stops_at_its_floor() {
        assert_eq!(
            angle(Step::Buffered(60_000)),
            angle(Step::Buffered(BUFFER_FULL_MS))
        );
        assert!(angle(Step::Buffered(0)) == angle(Step::Metadata));
    }

    #[test]
    fn a_late_or_repeated_step_never_turns_the_face_back() {
        let mut t = Turn::new(0.0);
        assert_eq!(t.hear(Step::Metadata, 10.0), Some(METADATA_DEG));
        assert_eq!(t.hear(Step::Negotiated, 20.0), None);
        assert_eq!(t.hear(Step::Metadata, 30.0), None);
        assert_eq!(t.step, Step::Metadata);
        assert_eq!(t.stepped_ms, 10.0);
        assert_eq!(t.hear(Step::FirstFrame, 40.0), Some(0.0));
    }

    #[test]
    fn a_quick_load_has_no_caption_and_a_slow_one_names_its_step() {
        let mut t = Turn::new(0.0);
        assert_eq!(t.caption(2_000.0), None);
        assert_eq!(t.caption(2_600.0), Some("Starting playback…"));
        t.hear(Step::Negotiated, 2_700.0);
        assert_eq!(t.caption(3_000.0), Some("Opening the stream…"));
        t.hear(Step::Buffered(500), 3_100.0);
        assert_eq!(t.caption(4_000.0), Some("Buffering…"));
    }

    #[test]
    fn a_load_that_stops_moving_says_it_is_waiting_on_the_server() {
        let mut t = Turn::new(0.0);
        t.hear(Step::Negotiated, 1_000.0);
        assert_eq!(t.caption(6_900.0), Some("Opening the stream…"));
        assert_eq!(t.caption(7_000.0), Some("Waiting for the server…"));
    }

    #[test]
    fn a_turn_with_no_first_frame_is_given_up() {
        let t = Turn::new(1_000.0);
        assert!(!t.given_up(30_999.0));
        assert!(t.given_up(31_000.0));
    }

    #[test]
    fn previous_turns_one_navigation_backwards() {
        assert_eq!(take_direction(), Direction::Forward);
        going_back();
        assert_eq!(take_direction(), Direction::Back);
        assert_eq!(take_direction(), Direction::Forward);
    }

    #[test]
    fn a_flat_face_is_the_identity_of_the_cube() {
        assert_eq!(
            face_transform(0.0),
            "translateZ(-50vw) rotateY(0.00deg) translateZ(50vw)"
        );
    }
}
