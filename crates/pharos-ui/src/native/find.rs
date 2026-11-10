//! The find screen's request rules, free of the browser: which typed text is
//! worth a `Library.search`, and whether a timer or reply still matters.

use pharos_client::api::{Item, ItemKind};

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

/// The search page's URL query for `term`, `?` included (FR-207): empty
/// for a blank term.
pub fn find_query(term: &str) -> String {
    let term = term.trim();
    if term.is_empty() {
        String::new()
    } else {
        format!("?q={}", super::browse::encode_component(term))
    }
}

/// The term a search page's URL query names; empty when it names none.
pub fn term_of(query: &str) -> String {
    query
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("q="))
        .map(|t| super::browse::decode_component(&t.replace('+', " ")))
        .unwrap_or_default()
}

/// The kinds of match the search page groups, in the order it shows them,
/// with each group's heading.
pub const GROUPS: [(ItemKind, &str); 12] = [
    (ItemKind::Movie, "Films"),
    (ItemKind::Series, "Shows"),
    (ItemKind::Season, "Seasons"),
    (ItemKind::Episode, "Episodes"),
    (ItemKind::Artist, "Artists"),
    (ItemKind::Album, "Albums"),
    (ItemKind::Audio, "Songs"),
    (ItemKind::Book, "Books"),
    (ItemKind::Person, "People"),
    (ItemKind::Collection, "Collections"),
    (ItemKind::Playlist, "Playlists"),
    (ItemKind::Channel, "Channels"),
];

/// `items` grouped by kind under their headings, in [`GROUPS`] order and
/// each group in the server's best-match order. Kinds outside the list
/// gather last under "Other". Groups with no match are left out.
pub fn grouped(items: &[Item]) -> Vec<(&'static str, Vec<Item>)> {
    let mut out: Vec<(&'static str, Vec<Item>)> = GROUPS
        .iter()
        .map(|(kind, heading)| {
            let of: Vec<Item> = items.iter().filter(|i| i.kind == *kind).cloned().collect();
            (*heading, of)
        })
        .collect();
    let other: Vec<Item> = items
        .iter()
        .filter(|i| !GROUPS.iter().any(|(kind, _)| *kind == i.kind))
        .cloned()
        .collect();
    out.push(("Other", other));
    out.retain(|(_, of)| !of.is_empty());
    out
}

/// What the search page says when it shows fewer matches than there are:
/// the server answers at most [`RESULT_LIMIT`], so the rest need a narrower
/// term. `None` when every match is shown.
pub fn cap_note(shown: usize, total: u32) -> Option<String> {
    (total as usize > shown).then(|| {
        format!("Showing the best {shown} of {total} matches. Add a word to narrow the search.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pharos_client::api::ItemRef;

    fn hit(id: u64, kind: ItemKind) -> Item {
        Item {
            id: ItemRef::Real(id),
            kind,
            name: format!("Hit {id}"),
            year: None,
            runtime_ms: 0,
            image: None,
            played: false,
            favorite: false,
            position_ms: 0,
            series_name: None,
            season: None,
            episode: None,
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
            artist: None,
            stereo: None,
            damaged: false,
        }
    }

    #[test]
    fn hits_group_by_kind_in_a_fixed_order_keeping_match_order() {
        let items = vec![
            hit(1, ItemKind::Episode),
            hit(2, ItemKind::Movie),
            hit(3, ItemKind::Library),
            hit(4, ItemKind::Episode),
        ];
        let groups = grouped(&items);
        let headings: Vec<&str> = groups.iter().map(|(h, _)| *h).collect();
        assert_eq!(headings, vec!["Films", "Episodes", "Other"]);
        let episodes: Vec<ItemRef> = groups[1].1.iter().map(|i| i.id).collect();
        assert_eq!(episodes, vec![ItemRef::Real(1), ItemRef::Real(4)]);
        assert!(grouped(&[]).is_empty());
    }

    #[test]
    fn group_headings_are_distinct() {
        let mut headings: Vec<&str> = GROUPS.iter().map(|(_, h)| *h).collect();
        headings.push("Other");
        let n = headings.len();
        headings.sort_unstable();
        headings.dedup();
        assert_eq!(headings.len(), n);
    }

    #[test]
    fn the_search_term_round_trips_through_the_url() {
        assert_eq!(find_query("  "), "");
        assert_eq!(find_query("a b&c"), "?q=a%20b%26c");
        assert_eq!(term_of(&find_query("a b&c")), "a b&c");
        assert_eq!(term_of("?q=two+words"), "two words");
        assert_eq!(term_of(""), "");
    }

    #[test]
    fn the_cap_is_stated_only_when_matches_are_left_out() {
        assert_eq!(cap_note(50, 50), None);
        assert_eq!(cap_note(3, 3), None);
        assert!(cap_note(50, 120).is_some_and(|n| n.contains("50 of 120")));
    }

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
