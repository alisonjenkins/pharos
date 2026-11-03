//! The find screen's request rules, free of the browser: which typed text is
//! worth a `Library.search`, and whether a timer or reply still matters.

/// How long typing must rest before a search goes out. Each keystroke would
/// otherwise be a round trip; a quarter second of rest reads as "done typing".
pub const DEBOUNCE_MS: u32 = 250;

/// How many matches one search asks for: a page the grid shows without a
/// second request, well under the server's `QUERY_MAX` cap.
pub const RESULT_LIMIT: u32 = 50;

/// How many matches the top bar's box lists under itself: enough to pick
/// from at a glance; the rest are a "see all" away on the find screen.
pub const SUGGEST_LIMIT: usize = 6;

/// The newest edit of the find box. Every edit takes a new generation, so a
/// timer or reply from an older edit is recognised as stale and dropped.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Find {
    generation: u64,
    term: String,
}

impl Find {
    /// Records `text` as the newest edit. Returns the generation a search for
    /// it runs under, or `None` when the trimmed text is blank: a blank box
    /// asks nothing, and any search still in flight is now stale.
    pub fn edit(&mut self, text: &str) -> Option<u64> {
        self.generation = self.generation.wrapping_add(1);
        self.term = text.trim().to_owned();
        (!self.term.is_empty()).then_some(self.generation)
    }

    /// The trimmed term of the newest edit.
    pub fn term(&self) -> &str {
        &self.term
    }

    /// Whether `generation` is still the newest edit.
    pub fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }
}

/// The suggestion an arrow key moves to among `len` of them, from `active`
/// (`None`: the box itself). Down from the box takes the first, up from the
/// first returns to the box, and both ends stop rather than wrap.
pub fn step(active: Option<usize>, len: usize, down: bool) -> Option<usize> {
    let last = len.checked_sub(1)?;
    match (active, down) {
        (None, true) => Some(0),
        (None, false) => None,
        (Some(at), true) => Some(at.saturating_add(1).min(last)),
        (Some(at), false) => at.checked_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_walk_the_suggestions_and_stop_at_the_ends() {
        assert_eq!(step(None, 3, true), Some(0));
        assert_eq!(step(Some(0), 3, true), Some(1));
        assert_eq!(step(Some(2), 3, true), Some(2));
        assert_eq!(step(Some(1), 3, false), Some(0));
        assert_eq!(
            step(Some(0), 3, false),
            None,
            "up from the first is the box"
        );
        assert_eq!(step(None, 3, false), None);
    }

    #[test]
    fn arrows_do_nothing_without_suggestions() {
        assert_eq!(step(None, 0, true), None);
        assert_eq!(step(Some(4), 0, false), None);
    }

    #[test]
    fn blank_text_asks_nothing() {
        let mut find = Find::default();
        assert_eq!(find.edit(""), None);
        assert_eq!(find.edit("  \t "), None);
    }

    #[test]
    fn the_term_is_trimmed() {
        let mut find = Find::default();
        assert_eq!(find.edit("  Alpha "), Some(1));
        assert_eq!(find.term(), "Alpha");
    }

    #[test]
    fn only_the_newest_edit_is_current() {
        let mut find = Find::default();
        let first = find.edit("Al");
        let second = find.edit("Alp");
        assert_eq!((first, second), (Some(1), Some(2)));
        assert!(
            !find.is_current(1),
            "a superseded debounce timer must not search"
        );
        assert!(find.is_current(2));
    }

    #[test]
    fn a_reply_for_an_older_term_is_dropped() {
        let mut find = Find::default();
        let sent_for_al = find.edit("Al").unwrap_or_default();
        find.edit("Alpha");
        assert!(
            !find.is_current(sent_for_al),
            "a late reply for 'Al' must not overwrite the results for 'Alpha'"
        );
    }

    #[test]
    fn clearing_the_box_makes_a_reply_in_flight_stale() {
        let mut find = Find::default();
        let sent = find.edit("Alpha").unwrap_or_default();
        assert_eq!(find.edit(""), None);
        assert!(!find.is_current(sent));
    }
}
