//! Paging a browse grid (V237): which page to ask for next, and when to stop;
//! and how far to read for a jump to a letter (V252).
//! Pure, so the rules run under host tests; the scroll sentinel and the fetch
//! that feed it live in `browse` and the platform module.

use pharos_client::api::{Item, Page};

/// How many tiles before the end of the loaded items the scroll sentinel
/// sits, so the next page is on its way before the viewer reaches the end.
pub const SENTINEL_LEAD: usize = 30;

/// What asked for a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The scroll sentinel came into view; never retries a failed page.
    Scroll,
    /// The "Show more" / "Try again" button.
    Button,
}

/// The items loaded so far and the state of the next page.
#[derive(Debug, Clone, PartialEq)]
pub struct Pager {
    items: Vec<Item>,
    total: u32,
    in_flight: bool,
    failed: bool,
    /// A page came back empty: `total` was wrong, so stop asking.
    exhausted: bool,
}

impl Pager {
    pub fn new(first: Page) -> Self {
        Self {
            items: first.items,
            total: first.total,
            in_flight: false,
            failed: false,
            exhausted: false,
        }
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn total(&self) -> u32 {
        self.total
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    fn loaded(&self) -> u32 {
        u32::try_from(self.items.len()).unwrap_or(u32::MAX)
    }

    /// Whether more items are worth asking for.
    pub fn has_more(&self) -> bool {
        !self.exhausted && self.loaded() < self.total
    }

    /// The index of the tile the scroll sentinel marks: `SENTINEL_LEAD`
    /// tiles before the end, or the first tile in a shorter list. `None`
    /// once nothing more can be loaded.
    pub fn sentinel(&self) -> Option<usize> {
        self.has_more()
            .then(|| self.items.len().saturating_sub(SENTINEL_LEAD))
    }

    /// Claims the next page: the start index to ask for, or `None` when a
    /// page is already in flight, nothing more can be loaded, or a failed
    /// page is awaiting a button press. At most one page is ever in flight.
    pub fn begin(&mut self, trigger: Trigger) -> Option<u32> {
        if self.in_flight || !self.has_more() || (self.failed && trigger == Trigger::Scroll) {
            return None;
        }
        self.in_flight = true;
        self.failed = false;
        Some(self.loaded())
    }

    /// Claims the pages up to and including the tile at `target`, for a
    /// jump to a letter (B312/V252): the start and how many to ask for, a
    /// whole number of `page`s but at most `max`. `None` when that tile is
    /// already loaded, a page is in flight, or nothing more can be loaded.
    /// Pages stay contiguous from the first, so scrolling back up needs
    /// nothing more.
    pub fn begin_through(&mut self, target: u32, page: u32, max: u32) -> Option<(u32, u32)> {
        let loaded = self.loaded();
        if self.in_flight || !self.has_more() || target < loaded {
            return None;
        }
        let want = (target - loaded + 1).div_ceil(page.max(1)) * page.max(1);
        self.in_flight = true;
        self.failed = false;
        Some((loaded, want.clamp(page.max(1), max.max(page))))
    }

    /// Appends the page that was asked for. An empty page ends paging.
    pub fn land(&mut self, page: Page) {
        self.in_flight = false;
        self.failed = false;
        if page.items.is_empty() {
            self.exhausted = true;
            return;
        }
        self.items.extend(page.items);
        self.total = page.total;
    }

    /// The page asked for failed; the same page is asked for again on the
    /// next button press.
    pub fn fail(&mut self) {
        self.in_flight = false;
        self.failed = true;
    }
}

#[cfg(test)]
mod tests {
    use pharos_client::api::{ItemKind, ItemRef};

    use super::*;

    fn items(from: u64, n: u64) -> Vec<Item> {
        (from..from + n)
            .map(|i| Item {
                id: ItemRef::Real(i),
                kind: ItemKind::Movie,
                name: format!("m{i}"),
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
                stereo: None,
            })
            .collect()
    }

    fn page(from: u64, n: u64, total: u32) -> Page {
        Page {
            items: items(from, n),
            total,
            start: 0,
        }
    }

    fn names(p: &Pager) -> Vec<&str> {
        p.items().iter().map(|i| i.name.as_str()).collect()
    }

    #[test]
    fn the_next_page_starts_after_what_is_loaded() {
        let mut p = Pager::new(page(0, 100, 250));
        assert_eq!(p.begin(Trigger::Scroll), Some(100));
        p.land(page(100, 100, 250));
        assert_eq!(p.begin(Trigger::Scroll), Some(200));
    }

    #[test]
    fn pages_merge_in_order() {
        let mut p = Pager::new(page(0, 2, 5));
        p.begin(Trigger::Scroll);
        p.land(page(2, 2, 5));
        p.begin(Trigger::Scroll);
        p.land(page(4, 1, 5));
        assert_eq!(names(&p), ["m0", "m1", "m2", "m3", "m4"]);
    }

    #[test]
    fn it_stops_at_the_total() {
        let mut p = Pager::new(page(0, 2, 3));
        p.begin(Trigger::Scroll);
        p.land(page(2, 1, 3));
        assert!(!p.has_more());
        assert_eq!(p.begin(Trigger::Scroll), None);
        assert_eq!(p.begin(Trigger::Button), None);
        assert_eq!(p.sentinel(), None);
    }

    #[test]
    fn a_complete_first_page_asks_for_nothing() {
        let mut p = Pager::new(page(0, 3, 3));
        assert_eq!(p.begin(Trigger::Button), None);
    }

    #[test]
    fn an_empty_page_stops_paging_even_when_the_total_says_more() {
        let mut p = Pager::new(page(0, 2, 10));
        assert_eq!(p.begin(Trigger::Scroll), Some(2));
        p.land(page(2, 0, 10));
        assert!(!p.has_more());
        assert_eq!(p.begin(Trigger::Button), None);
        assert_eq!(p.items().len(), 2);
    }

    #[test]
    fn a_scroll_and_a_click_at_once_ask_for_one_page() {
        let mut p = Pager::new(page(0, 2, 10));
        assert_eq!(p.begin(Trigger::Scroll), Some(2));
        assert!(p.in_flight());
        assert_eq!(p.begin(Trigger::Button), None);
        assert_eq!(p.begin(Trigger::Scroll), None);
        p.land(page(2, 2, 10));
        assert_eq!(p.begin(Trigger::Button), Some(4));
    }

    #[test]
    fn a_failed_page_is_asked_for_again_from_the_same_start() {
        let mut p = Pager::new(page(0, 2, 10));
        assert_eq!(p.begin(Trigger::Scroll), Some(2));
        p.fail();
        assert!(p.failed());
        assert!(!p.in_flight());
        assert_eq!(p.items().len(), 2);
        assert_eq!(p.begin(Trigger::Button), Some(2));
        assert!(!p.failed());
    }

    #[test]
    fn scrolling_does_not_retry_a_failed_page() {
        let mut p = Pager::new(page(0, 2, 10));
        p.begin(Trigger::Scroll);
        p.fail();
        assert_eq!(p.begin(Trigger::Scroll), None);
        assert!(p.failed());
    }

    #[test]
    fn the_sentinel_sits_thirty_tiles_before_the_end() {
        let p = Pager::new(page(0, 100, 250));
        assert_eq!(p.sentinel(), Some(70));
        let short = Pager::new(page(0, 10, 250));
        assert_eq!(short.sentinel(), Some(0));
    }

    #[test]
    fn a_jump_asks_for_whole_pages_through_its_target() {
        let mut p = Pager::new(page(0, 100, 5000));
        assert_eq!(p.begin_through(50, 100, 1000), None, "already loaded");
        assert_eq!(p.begin_through(100, 100, 1000), Some((100, 100)));
        assert_eq!(p.begin_through(900, 100, 1000), None, "one in flight");
        p.land(page(100, 100, 5000));
        assert_eq!(p.begin_through(950, 100, 1000), Some((200, 800)));
        p.land(page(200, 800, 5000));
        assert_eq!(
            p.begin_through(4000, 100, 1000),
            Some((1000, 1000)),
            "capped"
        );
    }

    #[test]
    fn a_jump_past_the_end_asks_for_nothing() {
        let mut p = Pager::new(page(0, 3, 3));
        assert_eq!(p.begin_through(10, 100, 1000), None);
    }

    #[test]
    fn a_landed_page_updates_the_total() {
        let mut p = Pager::new(page(0, 2, 4));
        p.begin(Trigger::Scroll);
        p.land(page(2, 2, 6));
        assert_eq!(p.total(), 6);
        assert!(p.has_more());
    }
}
