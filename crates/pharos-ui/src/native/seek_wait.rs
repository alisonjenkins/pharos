//! What a viewer is told while a seek waits on the server (B1196, V1156).
//!
//! A seek into a file the server has to read from its start (an AVI with no
//! seek index) can wait a minute for its first frame. "Waiting for the
//! server…" over a frozen picture reads as a hang; the card says where it is
//! going, how long it has waited, why when that is known, and offers the way
//! back to where the viewer was.

use super::playback::clock;

/// A seek waits this long before the card replaces the plain spinner: a seek
/// that lands sooner never flashes it.
pub const SHOW_AFTER_MS: u64 = 4_000;

/// A seek the viewer is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeekWait {
    /// Where the viewer was before the first of these seeks.
    pub from_ms: u64,
    /// Where the latest seek is going.
    pub to_ms: u64,
    /// When the latest seek started, in the page clock's ms.
    pub started_ms: u64,
}

impl SeekWait {
    /// A seek from `at_ms` to `to_ms` at `now_ms`. A seek made while an
    /// earlier one still waits keeps that one's starting point: "go back"
    /// means where the viewer was watching, not a target never reached.
    pub fn begin(earlier: Option<Self>, at_ms: u64, to_ms: u64, now_ms: u64) -> Self {
        Self {
            from_ms: earlier.map_or(at_ms, |e| e.from_ms),
            to_ms,
            started_ms: now_ms,
        }
    }

    /// The card to show at `now_ms`, once the seek has waited long enough.
    /// `unindexed` is the integrity scan's word that the file has no seek
    /// index, which is the one cause the player can name.
    pub fn card(&self, now_ms: u64, unindexed: bool) -> Option<SeekCard> {
        let waited = now_ms.saturating_sub(self.started_ms);
        (waited >= SHOW_AFTER_MS).then(|| SeekCard {
            title: format!("Jumping to {}…", clock(self.to_ms)),
            waited: format!("Waiting {} s", waited / 1000),
            reason: unindexed.then_some(
                "This file has no seek index, so the server reads it from the start to get \
                 here. That can take up to a minute.",
            ),
            back: format!("Go back to {}", clock(self.from_ms)),
            back_to_ms: self.from_ms,
        })
    }
}

/// The waiting card's text and its way back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekCard {
    pub title: String,
    pub waited: String,
    pub reason: Option<&'static str>,
    pub back: String,
    pub back_to_ms: u64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_quick_seek_shows_no_card() {
        let w = SeekWait::begin(None, 60_000, 5_730_000, 1_000);
        assert_eq!(w.card(1_000 + SHOW_AFTER_MS - 1, true), None);
    }

    #[test]
    fn a_slow_seek_says_where_how_long_and_the_way_back() {
        let w = SeekWait::begin(None, 4_805_000, 5_730_000, 1_000);
        let card = w.card(13_500, false).unwrap();
        assert_eq!(card.title, "Jumping to 1:35:30…");
        assert_eq!(card.waited, "Waiting 12 s");
        assert_eq!(card.reason, None);
        assert_eq!(card.back, "Go back to 1:20:05");
        assert_eq!(card.back_to_ms, 4_805_000);
    }

    #[test]
    fn an_unindexed_file_names_the_cause() {
        let w = SeekWait::begin(None, 0, 600_000, 0);
        let reason = w.card(5_000, true).unwrap().reason.unwrap();
        assert!(reason.contains("no seek index"), "{reason}");
    }

    #[test]
    fn a_seek_while_waiting_keeps_where_the_viewer_was() {
        let first = SeekWait::begin(None, 300_000, 900_000, 0);
        // The element reports the first target as its position meanwhile.
        let second = SeekWait::begin(Some(first), 900_000, 1_200_000, 6_000);
        assert_eq!(second.from_ms, 300_000);
        assert_eq!(second.to_ms, 1_200_000);
        assert_eq!(second.card(8_000, false), None, "the clock restarts");
        assert_eq!(second.card(10_000, false).unwrap().back, "Go back to 5:00");
    }
}
