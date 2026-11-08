//! Paging a browse grid (V237): which page to ask for next, and when to stop;
//! and how far to read for a jump to a letter (V252).
//! Pure, so the rules run under host tests; the scroll sentinel and the fetch
//! that feed it live in `browse` and the platform module.

use pharos_client::api::{Item, ItemKind, Listing, Page};

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
        let total = page.total;
        self.land_as(page, total);
    }

    /// As [`Pager::land`], for a listing read in two parts ([`Chain`]): the
    /// page's own total is only one part's, so the caller says the whole's.
    pub fn land_as(&mut self, page: Page, total: u32) {
        self.in_flight = false;
        self.failed = false;
        if page.items.is_empty() {
            self.exhausted = true;
            return;
        }
        self.items.extend(page.items);
        self.total = total;
    }

    /// The first page was read again (a watched mark reloaded the screen):
    /// replaces the items it covers with the fresh ones and keeps the pages
    /// loaded after them. Returns the range of those pages, `(start, count)`,
    /// for the caller to read again so their marks are fresh too; the pager
    /// counts that read as the page in flight until [`Pager::land_tail`].
    pub fn refresh_head(&mut self, head: Page) -> Option<(u32, u32)> {
        let covered = head.items.len();
        if self.items.len() <= covered {
            self.items = head.items;
            self.total = head.total;
            return None;
        }
        let tail = self.items.split_off(covered);
        let count = u32::try_from(tail.len()).unwrap_or(u32::MAX);
        let start = u32::try_from(covered).unwrap_or(u32::MAX);
        self.items = head.items;
        self.total = head.total;
        self.in_flight = true;
        self.failed = false;
        // The old tail stays on screen until the fresh one lands.
        self.items.extend(tail);
        Some((start, count))
    }

    /// The pages after the first, read again: replaces what was loaded from
    /// `start` on.
    pub fn land_tail(&mut self, start: u32, page: Page) {
        self.in_flight = false;
        let at = usize::try_from(start).unwrap_or(usize::MAX);
        if page.items.is_empty() || at > self.items.len() {
            return;
        }
        self.items.truncate(at);
        self.items.extend(page.items);
    }

    /// The page asked for failed; the same page is asked for again on the
    /// next button press.
    pub fn fail(&mut self) {
        self.in_flight = false;
        self.failed = true;
    }
}

/// Which listing of a [`Chain`] a position falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Head,
    Tail,
}

/// One grid read from two listings, the head's items then the tail's (a
/// mixed library: its series, then its other stored items). Positions run
/// over both as if they were one listing, so a [`Pager`] pages through them
/// unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct Chain {
    pub head: Listing,
    /// How many items the head listing holds.
    pub head_total: u32,
    pub tail: Listing,
    /// A kind the tail lists but the grid leaves out: the head stands for
    /// it (a series' episodes). The pager still counts it, so the positions
    /// stay the server's.
    pub hide: Option<ItemKind>,
}

impl Chain {
    /// Where the read of `limit` items from position `start` goes: the
    /// listing, its own start, and how many to ask for. A read never
    /// crosses from head to tail; the next one starts in the tail.
    pub fn step(&self, start: u32, limit: u32) -> (Part, u32, u32) {
        if start < self.head_total {
            (Part::Head, start, limit.min(self.head_total - start))
        } else {
            (Part::Tail, start - self.head_total, limit)
        }
    }

    pub fn listing(&self, part: Part) -> &Listing {
        match part {
            Part::Head => &self.head,
            Part::Tail => &self.tail,
        }
    }

    /// The total over both parts once a page of `part` said its own `total`,
    /// the other part's being `whole - this part's old size`.
    pub fn whole(&self, part: Part, page_total: u32, whole: u32) -> u32 {
        match part {
            Part::Head => whole.saturating_sub(self.head_total) + page_total,
            Part::Tail => self.head_total + page_total,
        }
    }

    /// The items the grid shows.
    pub fn shown<'a>(&self, items: &'a [Item]) -> Vec<&'a Item> {
        items.iter().filter(|i| Some(i.kind) != self.hide).collect()
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
                imdb_rating: None,
                critic_rating: None,
                artist: None,
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

    fn chain(head_total: u32) -> Chain {
        Chain {
            head: Listing::of(ItemRef::Real(1), Some(ItemKind::Series), "SortName"),
            head_total,
            tail: Listing::of(ItemRef::Real(1), None, "SortName"),
            hide: Some(ItemKind::Episode),
        }
    }

    #[test]
    fn a_chain_reads_the_head_then_the_tail_and_never_across() {
        let c = chain(150);
        assert_eq!(c.step(100, 100), (Part::Head, 100, 50));
        assert_eq!(c.step(150, 100), (Part::Tail, 0, 100));
        assert_eq!(c.step(250, 100), (Part::Tail, 100, 100));
        assert_eq!(chain(0).step(0, 100), (Part::Tail, 0, 100));
    }

    #[test]
    fn a_chain_totals_both_parts() {
        let c = chain(150);
        assert_eq!(c.whole(Part::Head, 150, 400), 400);
        assert_eq!(c.whole(Part::Tail, 300, 400), 450);
    }

    #[test]
    fn a_chain_hides_only_the_kind_its_head_stands_for() {
        let mut episode = items(0, 1);
        episode[0].kind = ItemKind::Episode;
        let mut all = items(1, 2);
        all.extend(episode);
        let shown = chain(1).shown(&all);
        assert_eq!(shown.len(), 2);
        assert!(shown.iter().all(|i| i.kind == ItemKind::Movie));
    }

    #[test]
    fn a_reloaded_head_keeps_the_pages_after_it_and_asks_for_them_again() {
        let mut p = Pager::new(page(0, 2, 6));
        p.begin(Trigger::Button);
        p.land(page(2, 2, 6));
        let mut fresh = page(0, 2, 6);
        fresh.items[0].played = true;
        assert_eq!(p.refresh_head(fresh), Some((2, 2)));
        assert!(p.items()[0].played);
        assert_eq!(names(&p), ["m0", "m1", "m2", "m3"], "the old tail stays");
        assert_eq!(p.begin(Trigger::Button), None, "the read is in flight");
        let mut again = page(2, 2, 6);
        again.items[1].played = true;
        p.land_tail(2, again);
        assert!(p.items()[3].played);
        assert!(!p.in_flight());
        assert_eq!(p.begin(Trigger::Button), Some(4));
    }

    #[test]
    fn a_reloaded_head_with_nothing_after_it_asks_for_nothing() {
        let mut p = Pager::new(page(0, 2, 2));
        assert_eq!(p.refresh_head(page(0, 2, 2)), None);
        assert!(!p.in_flight());
    }
}
