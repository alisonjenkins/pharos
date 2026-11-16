//! The grid of covers the album shelf and the ebook list share: the entries
//! loaded so far, the page showing, the letters to jump to, the covers worth
//! having and whether the page showing still waits on any.
//!
//! What differs between the two (the tile a row becomes, which actions it
//! carries, filters and menus) stays with the caller, which builds its tiles
//! from [`Shelf::page_tiles`].

use crate::letters;
use kobo_sdk::{action_id, ActionId, Context, ScreenBuilder, TileShape};

/// What a shelf needs to know of a row.
pub trait Item {
    fn id(&self) -> &str;
    /// Pharos's tag for the cover, present only when there is one to fetch.
    fn image_tag(&self) -> Option<&str>;
    /// The name the letters are filed under.
    fn sort_name(&self) -> &str;
}

#[derive(Debug)]
pub struct Shelf<T> {
    pub entries: Vec<T>,
    /// How many rows the listing holds in all.
    pub total: usize,
    shape: TileShape,
    /// How many rows of the listing have been asked for, kept or not.
    sent: usize,
    /// Whether the whole listing has been read.
    done: bool,
    /// How many entries the top bar counts. It catches up when the screen is
    /// drawn for something else, so a later block landing behind the page
    /// showing draws nothing of its own.
    counted: usize,
    page: usize,
    /// Whether the letters to jump to are showing (V1442).
    picking: bool,
}

impl<T> Default for Shelf<T> {
    fn default() -> Self {
        Self::new(TileShape::Square)
    }
}

impl<T> Shelf<T> {
    #[must_use]
    pub fn new(shape: TileShape) -> Self {
        Self {
            entries: Vec::new(),
            total: 0,
            shape,
            sent: 0,
            done: false,
            counted: 0,
            page: 0,
            picking: false,
        }
    }

    /// Empties the shelf and sets the shape its tiles are drawn in.
    pub fn open(&mut self, shape: TileShape) {
        *self = Self::new(shape);
    }

    /// Empties the shelf, keeping its shape.
    pub fn clear(&mut self) {
        self.open(self.shape);
    }

    /// Draws the tiles in another shape, over the entries held.
    pub fn set_shape(&mut self, shape: TileShape) {
        self.shape = shape;
    }

    #[must_use]
    pub fn shape(&self) -> TileShape {
        self.shape
    }

    /// How many rows have been asked for, which the next block starts at.
    #[must_use]
    pub fn sent(&self) -> usize {
        self.sent
    }

    /// Whether the whole listing has been read.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn finish(&mut self) {
        self.done = true;
    }

    /// Takes the block that began `start` rows in, which held `sent` rows of
    /// `total`, unless it is not the one the shelf waits for. The listing is
    /// done when a block is empty, everything is read, or `limit` rows are.
    pub fn take_block(
        &mut self,
        start: usize,
        sent: usize,
        total: usize,
        entries: impl IntoIterator<Item = T>,
        limit: usize,
    ) -> bool {
        if start != self.sent {
            return false;
        }
        self.sent = self.sent.saturating_add(sent);
        self.total = total;
        self.entries.extend(entries);
        self.done = sent == 0 || self.sent >= total || self.sent >= limit;
        true
    }

    /// Counts every entry loaded in the top bar.
    pub fn catch_up(&mut self) {
        self.counted = self.entries.len();
    }

    /// Whether the top bar already counts every entry loaded.
    #[must_use]
    pub fn is_caught_up(&self) -> bool {
        self.counted == self.entries.len()
    }

    #[must_use]
    pub fn counted(&self) -> usize {
        self.counted
    }

    /// The page showing, as an index into [`Self::pages`] before it is
    /// clamped to them.
    #[must_use]
    pub fn page_number(&self) -> usize {
        self.page
    }
}

impl<T: Item> Shelf<T> {
    /// The entries, split into the pages this panel holds.
    #[must_use]
    pub fn pages(&self, context: &Context) -> Vec<Vec<usize>> {
        context.paginate_tiles(self.entries.len(), self.shape, false)
    }

    /// How many entries one page holds.
    #[must_use]
    pub fn per_page(&self, context: &Context) -> usize {
        self.pages(context).first().map_or(0, Vec::len)
    }

    /// The page showing, kept inside the pages there are.
    fn showing(&self, pages: &[Vec<usize>]) -> usize {
        self.page.min(pages.len().saturating_sub(1))
    }

    /// The entries on the page showing, by their place in the list.
    #[must_use]
    pub fn page_indices(&self, context: &Context) -> Vec<usize> {
        let pages = self.pages(context);
        pages.get(self.showing(&pages)).cloned().unwrap_or_default()
    }

    /// The tiles of the page showing, each built by `build` from the entry's
    /// place in the list and the entry.
    pub fn page_tiles<R>(&self, context: &Context, build: impl Fn(usize, &T) -> R) -> Vec<R> {
        self.page_indices(context)
            .into_iter()
            .filter_map(|index| Some(build(index, self.entries.get(index)?)))
            .collect()
    }

    /// Turns a page on, or back, unless it is the last, or the first.
    pub fn turn(&mut self, context: &Context, forward: bool) {
        let pages = self.pages(context).len().max(1);
        if forward && self.page + 1 < pages {
            self.page += 1;
        } else if !forward && self.page > 0 {
            self.page -= 1;
        }
    }

    /// `(id, cover tag)` of an entry that has a cover.
    #[must_use]
    pub fn tagged(entry: &T) -> Option<(String, String)> {
        Some((entry.id().to_owned(), entry.image_tag()?.to_owned()))
    }

    /// The entries whose covers are worth having now: those on the page
    /// showing and the next one, so turning finds them ready.
    #[must_use]
    pub fn wanted(&self, context: &Context) -> Vec<&T> {
        let pages = self.pages(context);
        crate::api::wanted(&pages, self.showing(&pages))
            .into_iter()
            .filter_map(|index| self.entries.get(index))
            .collect()
    }

    /// The ids of the covers on the page showing that are tagged.
    #[must_use]
    pub fn page_covers(&self, context: &Context) -> Vec<String> {
        self.page_indices(context)
            .into_iter()
            .filter_map(|index| self.entries.get(index))
            .filter(|entry| entry.image_tag().is_some())
            .map(|entry| entry.id().to_owned())
            .collect()
    }

    /// Whether a cover on the page showing is still to come, `unready` saying
    /// of an id that it is asked for and not here yet, or not asked for yet.
    #[must_use]
    pub fn covers_pending(&self, context: &Context, unready: &dyn Fn(&str) -> bool) -> bool {
        self.page_covers(context).iter().any(|id| unready(id))
    }

    /// Whether the page showing is still loading: `block_pending` says the
    /// entries it needs are on their way, or a cover on it is still to come.
    #[must_use]
    pub fn loading(
        &self,
        context: &Context,
        block_pending: bool,
        unready: &dyn Fn(&str) -> bool,
    ) -> bool {
        block_pending || self.covers_pending(context, unready)
    }

    /// Where each letter starts (V1442).
    #[must_use]
    pub fn letter_starts(&self) -> Vec<(char, usize)> {
        letters::starts(self.entries.iter().map(Item::sort_name))
    }

    /// The letters the entries hold, over `screen` while the reader picks
    /// one. The letter of the page showing is marked.
    #[must_use]
    pub fn letter_picker(&self, context: &Context, screen: ScreenBuilder) -> ScreenBuilder {
        if !self.picking {
            return screen;
        }
        let showing = self
            .page_indices(context)
            .first()
            .and_then(|&index| self.entries.get(index))
            .map(|entry| letters::letter_of(entry.sort_name()));
        letters::picker(screen, &self.letter_starts(), showing)
    }

    /// Answers the letter picker: a letter turns to the page its first entry
    /// is on, and Back closes it. Whether the action was its.
    pub fn pick_letter(&mut self, context: &Context, action: ActionId) -> bool {
        if action == action_id("letters") {
            self.picking = true;
            return true;
        }
        if !self.picking {
            return false;
        }
        self.picking = false;
        if action == ActionId::BACK {
            return true;
        }
        if let Some(index) = letters::chosen(action, &self.letter_starts()) {
            if let Some(page) = letters::page_holding(&self.pages(context), index) {
                self.page = page;
            }
            return true;
        }
        // Anything else on the screen closes the picker and does its own job.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Row {
        id: String,
        tag: Option<String>,
        name: String,
    }

    impl Row {
        fn new(name: &str, tagged: bool) -> Self {
            Self {
                id: format!("id-{name}"),
                tag: tagged.then(|| "tag".to_owned()),
                name: name.to_owned(),
            }
        }
    }

    impl Item for Row {
        fn id(&self) -> &str {
            &self.id
        }
        fn image_tag(&self) -> Option<&str> {
            self.tag.as_deref()
        }
        fn sort_name(&self) -> &str {
            &self.name
        }
    }

    fn context() -> Context {
        kobo_sdk::AppRunner::new(crate::Pharos::default()).context()
    }

    fn filled(count: usize) -> Shelf<Row> {
        let mut shelf = Shelf::new(TileShape::Square);
        let rows = (0..count).map(|n| Row::new(&format!("Row {n:03}"), n % 2 == 0));
        assert!(shelf.take_block(0, count, count, rows, 100));
        shelf
    }

    #[test]
    fn a_block_is_taken_only_where_the_listing_stands() {
        let mut shelf = Shelf::<Row>::default();
        assert!(!shelf.take_block(5, 5, 10, [Row::new("A", false)], 100));
        assert!(shelf.entries.is_empty());
        assert!(shelf.take_block(0, 5, 10, [Row::new("A", false)], 100));
        assert_eq!((shelf.sent(), shelf.total, shelf.is_done()), (5, 10, false));
        assert!(!shelf.take_block(0, 5, 10, [Row::new("B", false)], 100));
        assert!(shelf.take_block(5, 5, 10, [Row::new("B", false)], 100));
        assert!(shelf.is_done(), "everything is read");
    }

    #[test]
    fn a_listing_is_done_when_a_block_is_empty_or_the_limit_is_read() {
        let mut shelf = Shelf::<Row>::default();
        assert!(shelf.take_block(0, 4, 100, [], 4));
        assert!(shelf.is_done(), "the limit is read");
        let mut shelf = Shelf::<Row>::default();
        assert!(shelf.take_block(0, 0, 100, [], 1000));
        assert!(shelf.is_done(), "an empty block ends it");
    }

    #[test]
    fn the_top_bar_count_catches_up_only_when_told() {
        let mut shelf = filled(3);
        assert!(!shelf.is_caught_up());
        shelf.catch_up();
        assert!(shelf.is_caught_up());
        assert_eq!(shelf.counted(), 3);
        assert!(shelf.take_block(3, 1, 10, [Row::new("Late", false)], 100));
        assert!(!shelf.is_caught_up(), "a later block lands uncounted");
        assert_eq!(shelf.counted(), 3);
    }

    #[test]
    fn opening_empties_the_shelf_and_clearing_keeps_the_shape() {
        let mut shelf = filled(3);
        shelf.picking = true;
        shelf.open(TileShape::Portrait);
        assert!(shelf.entries.is_empty());
        assert!(!shelf.picking);
        shelf.clear();
        assert_eq!(shelf.shape(), TileShape::Portrait);
    }

    #[test]
    fn pages_hold_every_entry_once_and_turning_stays_inside_them() {
        let context = context();
        let mut shelf = filled(120);
        let pages = shelf.pages(&context);
        assert!(pages.len() > 1);
        assert_eq!(pages.iter().map(Vec::len).sum::<usize>(), 120);
        assert_eq!(shelf.per_page(&context), pages[0].len());
        shelf.turn(&context, false);
        assert_eq!(shelf.page_number(), 0);
        for _ in 0..pages.len() + 3 {
            shelf.turn(&context, true);
        }
        assert_eq!(shelf.page_number(), pages.len() - 1);
        shelf.turn(&context, false);
        assert_eq!(shelf.page_number(), pages.len() - 2);
    }

    #[test]
    fn the_covers_wanted_are_the_page_showing_and_the_next() {
        let context = context();
        let mut shelf = filled(120);
        let pages = shelf.pages(&context);
        let want = shelf.wanted(&context).len();
        assert_eq!(want, pages[0].len() + pages[1].len());
        shelf.turn(&context, true);
        shelf.turn(&context, true);
        let last = pages.len() - 1;
        while shelf.page_number() < last {
            shelf.turn(&context, true);
        }
        assert_eq!(shelf.wanted(&context).len(), pages[last].len());
    }

    #[test]
    fn only_tagged_covers_on_the_page_are_waited_for() {
        let context = context();
        let shelf = filled(4);
        let ids = shelf.page_covers(&context);
        assert_eq!(ids, vec!["id-Row 000", "id-Row 002"]);
        assert!(shelf.covers_pending(&context, &|_| true));
        assert!(!shelf.covers_pending(&context, &|_| false));
        assert!(!shelf.covers_pending(&context, &|id| id == "id-Row 001"));
        assert_eq!(
            Shelf::tagged(&shelf.entries[0]),
            Some(("id-Row 000".to_owned(), "tag".to_owned()))
        );
        assert_eq!(Shelf::tagged(&shelf.entries[1]), None);
    }

    #[test]
    fn the_page_is_loading_while_a_block_or_a_cover_is_on_its_way() {
        let context = context();
        let shelf = filled(4);
        assert!(!shelf.loading(&context, false, &|_| false));
        assert!(shelf.loading(&context, true, &|_| false));
        assert!(shelf.loading(&context, false, &|_| true));
    }

    #[test]
    fn tiles_are_built_for_the_page_showing() {
        let context = context();
        let shelf = filled(3);
        let tiles = shelf.page_tiles(&context, |index, row| format!("{index}:{}", row.name));
        assert_eq!(tiles, vec!["0:Row 000", "1:Row 001", "2:Row 002"]);
    }

    #[test]
    fn a_letter_jumps_to_the_page_its_first_entry_is_on() {
        let context = context();
        let mut shelf = Shelf::new(TileShape::Square);
        let names = (0..60)
            .map(|n| Row::new(&format!("Apple {n:02}"), false))
            .chain((0..60).map(|n| Row::new(&format!("Zest {n:02}"), false)));
        assert!(shelf.take_block(0, 120, 120, names, 1000));
        assert_eq!(shelf.letter_starts(), vec![('A', 0), ('Z', 60)]);
        assert!(
            !shelf.pick_letter(&context, action_id("letter-Z")),
            "closed"
        );
        assert!(shelf.pick_letter(&context, action_id("letters")));
        assert!(shelf.pick_letter(&context, action_id("letter-Z")));
        let pages = shelf.pages(&context);
        assert_eq!(Some(shelf.page_number()), letters::page_holding(&pages, 60));
        assert!(
            !shelf.pick_letter(&context, action_id("letter-Z")),
            "closed"
        );
    }

    #[test]
    fn back_closes_the_picker_and_another_action_falls_through() {
        let context = context();
        let mut shelf = filled(3);
        assert!(shelf.pick_letter(&context, action_id("letters")));
        assert!(shelf.pick_letter(&context, ActionId::BACK));
        assert!(!shelf.picking);
        assert!(shelf.pick_letter(&context, action_id("letters")));
        assert!(!shelf.pick_letter(&context, action_id("tile-0")));
        assert!(!shelf.picking, "any other action closes it");
    }
}
