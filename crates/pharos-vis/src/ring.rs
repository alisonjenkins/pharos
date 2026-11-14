//! The frames made ahead of the playhead, and which one the playhead is on.

use std::collections::VecDeque;
use std::time::Duration;

use crate::frame::{Frame, FPS};

/// How far ahead of the playhead frames are made: 2 s.
pub const AHEAD_FRAMES: u64 = 2 * FPS;
/// What the ring holds at most: the window ahead and a little slack for the
/// frame under the playhead, about 200 KB.
pub const RING_CAPACITY: usize = AHEAD_FRAMES as usize + 8;

/// Consecutive frames from `first`; the oldest drops when it is full.
#[derive(Debug)]
pub struct FrameRing {
    first: u64,
    frames: VecDeque<Frame>,
}

impl Default for FrameRing {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameRing {
    pub fn new() -> Self {
        Self {
            first: 0,
            frames: VecDeque::with_capacity(RING_CAPACITY),
        }
    }

    /// Add frame `index`. An index that does not follow the last one starts
    /// the ring over there.
    pub fn push(&mut self, index: u64, frame: Frame) {
        if index != self.next_index() {
            self.frames.clear();
            self.first = index;
        }
        if self.frames.len() == RING_CAPACITY {
            self.frames.pop_front();
            self.first += 1;
        }
        self.frames.push_back(frame);
    }

    /// The index the next pushed frame continues from.
    pub fn next_index(&self) -> u64 {
        self.first + self.frames.len() as u64
    }

    pub fn get(&self, index: u64) -> Option<&Frame> {
        let at = usize::try_from(index.checked_sub(self.first)?).ok()?;
        self.frames.get(at)
    }

    /// Drop the frames before `index`: the playhead has passed them.
    pub fn discard_before(&mut self, index: u64) {
        while self.first < index && !self.frames.is_empty() {
            self.frames.pop_front();
            self.first += 1;
        }
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Frames held at or after `index`.
    pub fn ahead_of(&self, index: u64) -> u64 {
        self.next_index().saturating_sub(index.max(self.first))
    }
}

/// The frame the playhead is on: the player's last reported position plus the
/// monotonic time since it was reported.
pub fn playhead_frame(position_ms: u64, since_report: Duration) -> u64 {
    let ms = u128::from(position_ms) + since_report.as_millis();
    u64::try_from(ms * u128::from(FPS) / 1000).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u8) -> Frame {
        Frame {
            bins: vec![tag],
            wave: vec![],
        }
    }

    #[test]
    fn frames_are_found_by_index_and_the_oldest_drops_at_capacity() {
        let mut r = FrameRing::new();
        for i in 0..RING_CAPACITY as u64 + 3 {
            r.push(i, frame(i as u8));
        }
        assert_eq!(r.len(), RING_CAPACITY);
        assert!(r.get(2).is_none(), "dropped");
        assert_eq!(r.get(3).map(|f| f.bins[0]), Some(3));
        assert_eq!(r.next_index(), RING_CAPACITY as u64 + 3);
    }

    #[test]
    fn a_gap_in_the_index_starts_the_ring_over() {
        let mut r = FrameRing::new();
        r.push(0, frame(0));
        r.push(1, frame(1));
        r.push(40, frame(40));
        assert_eq!(r.len(), 1);
        assert!(r.get(1).is_none());
        assert_eq!(r.get(40).map(|f| f.bins[0]), Some(40));
    }

    #[test]
    fn discarding_before_the_playhead_keeps_the_rest() {
        let mut r = FrameRing::new();
        for i in 10..20 {
            r.push(i, frame(i as u8));
        }
        r.discard_before(15);
        assert!(r.get(14).is_none());
        assert_eq!(r.get(15).map(|f| f.bins[0]), Some(15));
        assert_eq!(r.ahead_of(15), 5);
        assert_eq!(
            r.ahead_of(0),
            5,
            "an index behind the ring counts from its start"
        );
        assert_eq!(r.ahead_of(99), 0);
    }

    #[test]
    fn the_playhead_is_the_report_plus_the_time_since() {
        assert_eq!(playhead_frame(1000, Duration::ZERO), 30);
        assert_eq!(playhead_frame(1000, Duration::from_millis(500)), 45);
        assert_eq!(playhead_frame(0, Duration::from_millis(33)), 0);
        assert_eq!(playhead_frame(0, Duration::from_millis(34)), 1);
    }
}
