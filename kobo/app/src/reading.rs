//! The Books screens: the list, a book's detail page, the download, and the
//! reader.
//!
//! A book is downloaded in pieces (`GET /Items/{id}/Download`, with the
//! account's header), checked, written to the app's shelf, and opened with
//! `kobo-bookview`. The shelf file is named after the book, so a downloaded
//! book is a record in the `books-v1` list and a file under that name. Where
//! the reader stopped is kept in the same record, as `calibre-web` does.

use crate::about;
use crate::api::{self, Account};
use crate::books::{self, Book, Kind, Piece, Saved, Shelf, Support};
use crate::filters::{self, Narrow, Order, ReadState};
use crate::shelf;
use crate::MAX_JSON;
use kobo_bookview::{BookView, ShelfFault, ShelfStep, Step};
use kobo_read::{Memory, Outcome};
use kobo_sdk::{
    action_id, ActionId, BannerLevel, Context, Glyph, Header, LogLevel, Screen, ScreenBuilder,
    ShelfDownload, ShelfProgress, StoreResult, Task, TaskError, TaskId, TaskOutcome, TilePicture,
    TileShape, TileState,
};
use std::time::Instant;

/// How wide the cover is on a book's detail page, in millimetres: the same as
/// an album's masthead, so the two detail pages look alike.
const DETAIL_COVER_MM: u16 = 26;
/// The most characters of a description on a book's detail page. Only a few
/// buttons sit beneath it, so it can run to about eight lines of the
/// smallest reader's text before the rest moves to its About pages.
const DETAIL_TEASER_CHARS: usize = 480;

/// Where the caller goes after the Books screens handled something.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Leave {
    Stay,
    /// Back to the shelf it was opened from.
    Back,
    /// The server no longer knows this sign-in.
    SignOut,
}

#[derive(Debug, Default, PartialEq)]
enum Load {
    #[default]
    Unloaded,
    Loading,
    Ready,
    /// The record could not be read. Nothing is written over it.
    Blocked,
}

/// The most bytes of an opening taken: pharos sends about 6000 characters.
const MAX_OPENING: u32 = 64 * 1024;

/// Shown on the detail page when pharos has no opening for the book.
const NO_OPENING: &str = "This book has no opening Pharos can show.";

/// What the screen is showing, when no book is open.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    List,
    /// The menu of the book at this index in the list.
    Menu(usize),
    /// A page of the description of the book at this index.
    About { index: usize, page: usize },
    /// A page of the opening of the book at this index, held in
    /// [`Reading::opening`].
    Opening { index: usize, page: usize },
    /// The filter screen, changing [`Reading::draft`] (V1451).
    Filter,
    /// This page of the subjects to narrow to.
    Subjects(u16),
}

/// The opening text of the book whose detail page is open. Dropped when that
/// page is left.
#[derive(Default)]
enum Opening {
    #[default]
    None,
    Loading(TaskId),
    Ready(String),
}

/// The one piece of work that may be running.
#[derive(Default)]
enum Job {
    #[default]
    Idle,
    /// A piece is being fetched; `received` bytes are already on the shelf.
    Downloading {
        task: Option<TaskId>,
        received: usize,
    },
    /// A fetched piece is being written. The book is never held whole: each
    /// piece goes to the shelf before the next is asked for.
    Writing {
        /// What the shelf holds once this piece lands.
        received: usize,
        /// This piece finishes the file.
        last: bool,
    },
    Opening(ShelfDownload),
    /// An EPUB is being opened off the shelf a part at a time; the view
    /// asks for each part.
    OpeningShelf(Instant),
    Removing(String),
}

#[allow(clippy::struct_excessive_bools)] // independent flags, each documented
pub struct Reading {
    server: &'static str,
    version: &'static str,
    account: Option<Account>,
    load: Load,
    shelf: Shelf,
    list: shelf::Shelf<Book>,
    list_task: Option<(TaskId, usize)>,
    list_failed: Option<String>,
    /// How the list is narrowed and ordered (V1451).
    narrow: Narrow,
    /// The narrowing being chosen on the filter screen, applied by its
    /// Show books button.
    draft: Narrow,
    /// The subjects pharos's books carry, once asked for.
    subjects: Option<Vec<String>>,
    subjects_task: Option<TaskId>,
    subjects_failed: bool,
    mode: Mode,
    notice: Option<(BannerLevel, String)>,
    /// The book being downloaded, opened or removed.
    current: Option<Book>,
    /// A book whose saved file was unreadable: the next open downloads again.
    damaged: Option<String>,
    job: Job,
    view: BookView,
    opened: bool,
    /// The record being written, until the runtime answers.
    write: Option<Vec<u8>>,
    /// A change came while a write was out; write again when it lands.
    dirty: bool,
    /// Books on the reader that are not in the list loaded so far, asked
    /// for by id so the home screen can show them (V1445).
    known: Vec<Book>,
    known_task: Option<TaskId>,
    /// Whether the open book came from the home screen, which closing it
    /// returns to.
    from_home: bool,
    /// The book whose detail page the home screen opened, which need not be
    /// in the list loaded so far. While set, the detail and About pages
    /// show it rather than a book of the list.
    home_book: Option<Book>,
    opening: Opening,
}

impl Reading {
    #[must_use]
    pub fn new(server: &'static str, version: &'static str) -> Self {
        Self {
            server,
            version,
            account: None,
            load: Load::Unloaded,
            shelf: Shelf::default(),
            list: shelf::Shelf::new(TileShape::Portrait),
            list_task: None,
            list_failed: None,
            narrow: Narrow::default(),
            draft: Narrow::default(),
            subjects: None,
            subjects_task: None,
            subjects_failed: false,
            mode: Mode::List,
            notice: None,
            current: None,
            damaged: None,
            job: Job::Idle,
            view: BookView::default(),
            opened: false,
            write: None,
            dirty: false,
            known: Vec::new(),
            known_task: None,
            from_home: false,
            home_book: None,
            opening: Opening::None,
        }
    }

    /// Forgets everything of the signed-in account.
    pub fn reset(&mut self, context: &mut Context) {
        match &self.job {
            Job::Downloading { task, .. } => {
                if let Some(task) = task {
                    context.cancel(*task);
                }
                self.abandon_download(context);
            }
            Job::Writing { .. } => self.abandon_download(context),
            _ => {}
        }
        self.view.close(context);
        *self = Self::new(self.server, self.version);
    }

    /// Shows the Books screens for `account`, loading what they need once.
    pub fn open(&mut self, context: &mut Context, account: &Account) {
        self.prepare(context, account);
        self.mode = Mode::List;
        self.notice = None;
        self.from_home = false;
        self.home_book = None;
        self.drop_opening(context);
        if self.list.entries.is_empty() {
            self.load_list(context, 0);
        }
    }

    /// Reads the record of downloaded books, if it has not been, so the home
    /// screen knows what was read here.
    pub fn prepare(&mut self, context: &mut Context, account: &Account) {
        self.account = Some(account.clone());
        if self.load == Load::Unloaded {
            self.load = Load::Loading;
            context.store().load(books::KEY);
        }
    }

    /// The book `id`, from the list or from those asked for by id.
    fn book(&self, id: &str) -> Option<&Book> {
        self.list
            .entries
            .iter()
            .chain(&self.known)
            .find(|book| book.id == id)
    }

    /// Whether the book list is showing and a block of it is on its way.
    /// A book's detail page opened from home does not wait on the list.
    #[must_use]
    pub fn list_loading(&self) -> bool {
        !self.opened && self.mode == Mode::List && self.list_task.is_some()
    }

    /// Whether the home screen's Continue reading row is still waiting on
    /// pharos describing the books read here. The record of them is local
    /// and is not waited for.
    #[must_use]
    pub fn recent_loading(&self) -> bool {
        self.known_task.is_some()
    }

    /// The books read on this reader, the most recently read first, as far
    /// as pharos has described them (V1445).
    #[must_use]
    pub fn recent(&self) -> Vec<Book> {
        self.shelf
            .recent()
            .filter_map(|saved| self.book(&saved.id).cloned())
            .collect()
    }

    /// Asks pharos about the books read here that it has not described yet.
    pub fn fetch_known(&mut self, context: &mut Context) {
        if self.known_task.is_some() {
            return;
        }
        let Some(account) = &self.account else {
            return;
        };
        let missing: Vec<&str> = self
            .shelf
            .recent()
            .map(|saved| saved.id.as_str())
            .filter(|id| self.book(id).is_none())
            .collect();
        if missing.is_empty() {
            return;
        }
        self.known_task = context.spawn(Task::Fetch {
            url: books::by_ids_url(self.server, account, &missing),
            offset: 0,
            max_bytes: MAX_JSON,
            credential: None,
            headers: self.headers(),
        });
    }

    /// Opens the book `id`, read here before, from the home screen: where
    /// the reader stopped, and closing it goes back home. `false` when it
    /// is not one of [`Self::recent`].
    pub fn continue_book(&mut self, context: &mut Context, account: &Account, id: &str) -> bool {
        let Some(book) = self.recent().into_iter().find(|book| book.id == id) else {
            return false;
        };
        self.open(context, account);
        self.from_home = true;
        self.begin_book(context, book);
        true
    }

    /// Opens the detail page of the book `id`, read here before, from the
    /// home screen; Back from it, or closing the book read from it, goes
    /// back home. `false` when it is not one of [`Self::recent`].
    pub fn show_book(&mut self, context: &mut Context, account: &Account, id: &str) -> bool {
        let Some(book) = self.recent().into_iter().find(|book| book.id == id) else {
            return false;
        };
        self.open(context, account);
        self.from_home = true;
        self.home_book = Some(book);
        self.mode = Mode::Menu(0);
        true
    }

    /// The book a detail or About page at `index` shows.
    fn detail_book(&self, index: usize) -> Option<&Book> {
        self.home_book
            .as_ref()
            .or_else(|| self.list.entries.get(index))
    }

    fn headers(&self) -> Vec<Header> {
        let token = self.account.as_ref().map(|account| account.token.as_str());
        vec![Header::new(
            "X-Emby-Authorization",
            api::client_header_value(self.version, token),
        )]
    }

    fn load_list(&mut self, context: &mut Context, start: usize) {
        let Some(account) = &self.account else {
            return;
        };
        if self.list_task.is_some() {
            return;
        }
        self.list_failed = None;
        let downloaded: Vec<&str> = self
            .shelf
            .saved
            .iter()
            .map(|saved| saved.id.as_str())
            .collect();
        if self.narrow.downloaded && downloaded.is_empty() {
            // Nothing is on this reader, so there is nothing to ask for.
            self.list.restart();
            return;
        }
        let task = context.spawn(Task::Fetch {
            url: books::list_url(self.server, account, start, &self.narrow, &downloaded),
            offset: 0,
            max_bytes: MAX_JSON,
            credential: None,
            headers: self.headers(),
        });
        match task {
            Some(task) => self.list_task = Some((task, start)),
            None => self.list_failed = Some("The book list could not be requested.".to_owned()),
        }
    }

    fn say(&mut self, level: BannerLevel, text: impl Into<String>) {
        self.notice = Some((level, text.into()));
    }

    fn busy(&self) -> bool {
        !matches!(self.job, Job::Idle)
    }

    fn downloaded(&self, book: &Book) -> bool {
        self.shelf.get(&book.id).is_some()
    }

    /// Whether the list is the screen, so its covers are wanted and waited
    /// for: not while a book is open or being fetched, nor under a menu.
    fn list_showing(&self) -> bool {
        !(self.opened || self.mode != Mode::List || self.busy())
    }

    /// The covers worth having now, as `(book id, cover tag)`: an open
    /// detail page's first, then the list page showing and the next one, so
    /// turning, or going back to the list, finds them ready.
    #[must_use]
    pub fn wanted_covers(&self, context: &Context) -> Vec<(String, String)> {
        if self.opened || self.busy() {
            return Vec::new();
        }
        let detail = match self.mode {
            Mode::List => None,
            // The list may change under these, so nothing is fetched for it.
            Mode::Filter | Mode::Subjects(_) => return Vec::new(),
            Mode::Menu(index) | Mode::About { index, .. } | Mode::Opening { index, .. } => {
                self.detail_book(index)
            }
        };
        let listed = self
            .list
            .wanted(context)
            .into_iter()
            .filter(|book| detail.is_none_or(|detail| detail.id != book.id));
        detail
            .into_iter()
            .chain(listed)
            .filter_map(shelf::Shelf::tagged)
            .collect()
    }

    /// Whether a cover on the page showing is still to come, `unready` saying
    /// of a cover's id that it is asked for and not here yet, or not asked
    /// for yet.
    #[must_use]
    pub fn covers_pending(&self, context: &Context, unready: &dyn Fn(&str) -> bool) -> bool {
        self.list_showing() && self.list.covers_pending(context, unready)
    }

    /// Whether the screen showing still waits on the list: a block of it is
    /// on its way, or a cover on the page showing is still to come.
    #[must_use]
    pub fn loading(&self, context: &Context, unready: &dyn Fn(&str) -> bool) -> bool {
        self.list.loading(context, self.list_loading(), &|id| {
            self.list_showing() && unready(id)
        })
    }

    /// How many covers one page holds.
    #[must_use]
    pub fn per_page(&self, context: &Context) -> usize {
        self.list.per_page(context)
    }

    /// The tiles of the page showing. A book this reader cannot open says
    /// so under its title, in place of its author, and stays tappable so a
    /// tap can say why; a downloaded one carries the held mark and its menu.
    #[allow(clippy::type_complexity)]
    fn tiles(
        &self,
        context: &Context,
        cover: &dyn Fn(&str) -> Option<TilePicture>,
    ) -> Vec<(
        String,
        String,
        String,
        Glyph,
        String,
        Option<TilePicture>,
        TileState,
    )> {
        self.list.page_tiles(context, |index, book| {
            let subtitle = match book.support {
                Support::Cannot(_) => "Can't open here".to_owned(),
                Support::Reads(_) => book.author.clone(),
            };
            let (menu, state) = if self.downloaded(book) {
                (format!("book-menu-{index}"), TileState::Held)
            } else {
                (String::new(), TileState::Normal)
            };
            (
                format!("book-{index}"),
                book.title.clone(),
                subtitle,
                Glyph::Book,
                menu,
                cover(&book.id),
                state,
            )
        })
    }

    /// Answers the letter picker on the list. Whether the action was its.
    fn pick_letter(&mut self, context: &Context, action: ActionId) -> bool {
        self.mode == Mode::List && self.list.pick_letter(context, action)
    }

    fn banner(&self, screen: ScreenBuilder) -> ScreenBuilder {
        match &self.notice {
            Some((level, text)) => screen.banner(*level, text),
            None => screen,
        }
    }

    fn busy_screen(&self) -> Option<Screen> {
        match &self.job {
            Job::Downloading { received, .. } | Job::Writing { received, .. } => {
                let received = u64::try_from(*received).unwrap_or(u64::MAX);
                let received = received - received % books::PROGRESS_STEP;
                return Some(own(self
                    .banner(ScreenBuilder::new("pharos-book-busy").top_bar("Downloading"))
                    .transfer("Downloading the book", received, None)
                    .button("books-cancel", "Cancel")));
            }
            Job::Opening(_) | Job::OpeningShelf(_) => {
                return Some(own(ScreenBuilder::new("pharos-book-busy")
                    .top_bar("Opening")
                    .activity("Opening the book", None)));
            }
            Job::Removing(_) => {
                return Some(own(ScreenBuilder::new("pharos-book-busy")
                    .top_bar("Removing")
                    .activity("Removing the download", None)));
            }
            Job::Idle => {}
        }
        None
    }

    #[must_use]
    pub fn screen(&self, context: &Context, cover: &dyn Fn(&str) -> Option<TilePicture>) -> Screen {
        if self.opened {
            let title = self
                .current
                .as_ref()
                .map_or("Book", |book| book.title.as_str());
            if let Some(screen) = self.view.screen(title) {
                // Back closes the book onto the list; left to the platform it
                // would leave Pharos for the launcher.
                return screen.with_own_back(true);
            }
        }
        if let Some(screen) = self.busy_screen() {
            return screen;
        }
        if let Mode::Menu(index) = self.mode {
            if let Some(book) = self.detail_book(index) {
                return own(self.detail(book, cover(&book.id)));
            }
        }
        if let Mode::About { index, page } = self.mode {
            if let Some(book) = self.detail_book(index) {
                return own(about::screen(
                    context,
                    "pharos-book-about",
                    &book.title,
                    &book.overview,
                    page,
                    ("book-about-back", "book-about-next"),
                ));
            }
        }
        if let Mode::Opening { index, page } = self.mode {
            if self.detail_book(index).is_some() {
                let text = match &self.opening {
                    Opening::Ready(text) => text.as_str(),
                    Opening::Loading(_) | Opening::None => "Loading the opening…",
                };
                return own(about::screen(
                    context,
                    "pharos-book-opening",
                    "Opening",
                    text,
                    page,
                    ("book-opening-back", "book-opening-next"),
                ));
            }
        }
        if self.mode == Mode::Filter {
            return own(self.filter_screen());
        }
        if let Mode::Subjects(page) = self.mode {
            return own(self.subjects_screen(page));
        }
        if let Some(why) = &self.list_failed {
            return own(ScreenBuilder::new("pharos-books")
                .top_bar("Books")
                .error_state(why.clone())
                .primary_button("books-retry", "Try again"));
        }
        if self.list.entries.is_empty() {
            let screen = self.banner(
                ScreenBuilder::new("pharos-books")
                    .top_bar("Books")
                    .top_bar_action("books-filter", "Filter"),
            );
            return own(if self.list_task.is_some() {
                screen.activity("Loading your books", None)
            } else if self.narrow.narrows() {
                screen
                    .text("No books match these filters.")
                    .button("books-filter-reset", "Clear filters")
            } else {
                screen.text("Pharos has no books for this account.")
            });
        }
        let title = match self.narrow.summary() {
            Some(summary) => format!("{summary} · {}", self.list.total),
            None => format!("Books · {}", self.list.total),
        };
        let mut screen = ScreenBuilder::new("pharos-books").top_bar(title);
        // The letters follow name order; in a date order they would jump
        // about, so they are offered only by name.
        if self.narrow.order == Order::Name {
            screen = screen.top_bar_action("letters", "A–Z");
        }
        let mut screen = self.banner(screen.top_bar_action("books-filter", "Filter"));
        if self.load == Load::Blocked {
            screen = screen
                .banner(
                    BannerLevel::Error,
                    "Your downloaded books could not be read. They are kept.",
                )
                .banner_action("books-reload", "Try again");
        }
        own(screen
            .contextual_picture_tiles(TileShape::Portrait, self.tiles(context, cover))
            .page_turns("books-page-back", "books-page-next")
            .compose(|screen| self.list.letter_picker(context, screen)))
    }

    /// The filter screen: the order, the read state, the subject and
    /// Downloaded only, as chosen so far (V1451).
    fn filter_screen(&self) -> ScreenBuilder {
        let draft = &self.draft;
        let order = Order::ALL
            .iter()
            .position(|o| *o == draft.order)
            .unwrap_or(0);
        let read = ReadState::ALL
            .iter()
            .position(|r| *r == draft.read)
            .unwrap_or(0);
        let subject = draft.subject.clone().unwrap_or_else(|| "All".to_owned());
        ScreenBuilder::new("pharos-book-filter")
            .top_bar("Filter books")
            .section("Sort by")
            .segmented(order, Order::ALL.map(|o| (o.action(), o.label())))
            .section("Read")
            .segmented(read, ReadState::ALL.map(|r| (r.action(), r.label())))
            .section_with_value("Subject", subject)
            .section_link("books-subjects", "Choose")
            .toggle(
                "books-downloaded",
                "Downloaded only",
                "Books saved on this reader",
                draft.downloaded,
            )
            .primary_button("books-filter-apply", "Show books")
            .button("books-filter-clear", "Clear filters")
    }

    /// The subjects to narrow to, a page at a time, All first.
    fn subjects_screen(&self, page: u16) -> ScreenBuilder {
        let screen = ScreenBuilder::new("pharos-book-subjects").top_bar("Subject");
        let Some(subjects) = &self.subjects else {
            return if self.subjects_failed {
                screen
                    .error_state("The subjects could not be loaded.")
                    .primary_button("books-subjects-retry", "Try again")
            } else {
                screen.activity("Loading the subjects", None)
            };
        };
        let chosen = self
            .draft
            .subject
            .as_ref()
            .and_then(|subject| subjects.iter().position(|s| s == subject))
            .map_or(0, |index| index + 1);
        let options = std::iter::once((
            "books-subject-0".to_owned(),
            "All subjects".to_owned(),
            String::new(),
        ))
        .chain(subjects.iter().enumerate().map(|(index, name)| {
            (
                format!("books-subject-{}", index + 1),
                name.clone(),
                String::new(),
            )
        }));
        screen
            .option_list(
                "Show books on",
                page,
                "books-subjects-back",
                "books-subjects-next",
                options,
            )
            .chosen(chosen)
    }

    /// Asks pharos for the subjects, once, unless they are here or on
    /// their way.
    fn fetch_subjects(&mut self, context: &mut Context) {
        if self.subjects.is_some() || self.subjects_task.is_some() {
            return;
        }
        let Some(account) = &self.account else {
            return;
        };
        self.subjects_failed = false;
        self.subjects_task = context.spawn(Task::Fetch {
            url: filters::subjects_url(self.server, &account.user_id),
            offset: 0,
            max_bytes: MAX_JSON,
            credential: None,
            headers: self.headers(),
        });
        if self.subjects_task.is_none() {
            self.subjects_failed = true;
        }
    }

    /// Lists the books as [`Self::draft`] says, from the first, if that
    /// changes anything.
    fn apply_narrow(&mut self, context: &mut Context) {
        self.mode = Mode::List;
        if self.draft == self.narrow {
            return;
        }
        self.narrow = self.draft.clone();
        if let Some((task, _)) = self.list_task.take() {
            context.cancel(task);
        }
        self.list.clear();
        self.load_list(context, 0);
    }

    /// Answers the filter and subject screens. Whether the action was
    /// theirs.
    fn filter_action(&mut self, context: &mut Context, action: ActionId) -> bool {
        if action == action_id("books-filter") && self.mode == Mode::List {
            self.list.stop_picking();
            self.draft = self.narrow.clone();
            self.mode = Mode::Filter;
            self.fetch_subjects(context);
            return true;
        }
        if action == action_id("books-filter-reset") && self.mode == Mode::List {
            self.draft = Narrow::default();
            self.apply_narrow(context);
            return true;
        }
        match self.mode {
            Mode::Filter => {
                if action == ActionId::BACK {
                    self.mode = Mode::List;
                } else if action == action_id("books-filter-apply") {
                    self.apply_narrow(context);
                } else if action == action_id("books-filter-clear") {
                    self.draft = Narrow::default();
                } else if action == action_id("books-downloaded") {
                    self.draft.downloaded = !self.draft.downloaded;
                } else if action == action_id("books-subjects") {
                    // A failed ask waits for its Try again on that screen.
                    if !self.subjects_failed {
                        self.fetch_subjects(context);
                    }
                    self.mode = Mode::Subjects(self.subject_page(context));
                } else if let Some(order) = Order::ALL
                    .into_iter()
                    .find(|o| action == action_id(o.action()))
                {
                    self.draft.order = order;
                } else if let Some(read) = ReadState::ALL
                    .into_iter()
                    .find(|r| action == action_id(r.action()))
                {
                    self.draft.read = read;
                } else {
                    return false;
                }
                true
            }
            Mode::Subjects(page) => {
                if action == ActionId::BACK {
                    self.mode = Mode::Filter;
                } else if action == action_id("books-subjects-retry") {
                    self.fetch_subjects(context);
                } else if action == action_id("books-subjects-next") {
                    let last = self.subject_pages(context).saturating_sub(1);
                    self.mode = Mode::Subjects(page.saturating_add(1).min(last));
                } else if action == action_id("books-subjects-back") {
                    self.mode = Mode::Subjects(page.saturating_sub(1));
                } else if let Some(row) = self.subject_row(action) {
                    self.draft.subject = row
                        .checked_sub(1)
                        .and_then(|index| self.subjects.as_ref()?.get(index).cloned());
                    self.mode = Mode::Filter;
                } else {
                    return false;
                }
                true
            }
            _ => false,
        }
    }

    /// Which row of the subjects screen a tap names: 0 is All, then each
    /// subject in order.
    fn subject_row(&self, action: ActionId) -> Option<usize> {
        let count = self.subjects.as_ref()?.len();
        (0..=count).find(|row| action == action_id(&format!("books-subject-{row}")))
    }

    /// How many pages the subjects take, All among them.
    fn subject_pages(&self, context: &Context) -> u16 {
        let count = self.subjects.as_ref().map_or(0, Vec::len) + 1;
        u16::try_from(kobo_sdk::options_pages(count, &context.metrics(), false)).unwrap_or(u16::MAX)
    }

    /// The page holding the subject chosen so far.
    fn subject_page(&self, context: &Context) -> u16 {
        let Some(subjects) = &self.subjects else {
            return 0;
        };
        let index = self
            .draft
            .subject
            .as_ref()
            .and_then(|subject| subjects.iter().position(|s| s == subject))
            .map_or(0, |index| index + 1);
        let per_page = kobo_sdk::options_per_page(&context.metrics(), false).max(1);
        u16::try_from(index / per_page).unwrap_or(0)
    }

    /// A book's detail page: its cover beside the title and author, the
    /// start of its description, and what can be done with it.
    fn detail(&self, book: &Book, cover: Option<TilePicture>) -> ScreenBuilder {
        let by = (!book.author.is_empty()).then(|| book.author.clone());
        let downloaded = self.downloaded(book);
        let facts = if downloaded {
            vec![("On this reader", "Downloaded".to_owned())]
        } else {
            Vec::new()
        };
        let mut screen = self
            .banner(ScreenBuilder::new("pharos-book-detail").top_bar("Books"))
            .hero(cover, DETAIL_COVER_MM, book.title.clone(), by, facts);
        let teaser = about::teaser(&book.overview, DETAIL_TEASER_CHARS);
        if let Some((shown, _)) = &teaser {
            screen = screen.text(shown.clone());
        }
        screen = match book.support {
            Support::Cannot(why) => screen.text(why),
            Support::Reads(_) if downloaded => screen
                .primary_button("book-read", "Read")
                .button("book-remove", "Remove download"),
            Support::Reads(_) => screen.primary_button("book-read", "Download and read"),
        };
        if matches!(teaser, Some((_, true))) {
            screen = screen.button("book-about", "About this book");
        }
        if matches!(book.support, Support::Reads(_)) {
            screen = screen.button("book-opening", "Read the opening");
        }
        screen
    }

    /// The answer to loading the book list record.
    pub fn loaded(&mut self, result: &StoreResult) {
        let StoreResult::Loaded { value, .. } = result else {
            self.load = Load::Blocked;
            self.say(
                BannerLevel::Error,
                "Your downloaded books could not be read.",
            );
            return;
        };
        match value.as_deref() {
            None => {
                self.shelf = Shelf::default();
                self.load = Load::Ready;
            }
            Some(bytes) => {
                if let Some(decoded) = Shelf::decode(bytes) {
                    self.shelf = decoded;
                    self.load = Load::Ready;
                } else {
                    self.load = Load::Blocked;
                    self.say(
                        BannerLevel::Error,
                        "Your downloaded books could not be read. They are kept.",
                    );
                }
            }
        }
    }

    /// The answer to saving the book list record.
    pub fn saved(&mut self, context: &mut Context, result: &StoreResult) {
        self.write = None;
        if matches!(result, StoreResult::Saved { .. }) {
            if std::mem::take(&mut self.dirty) {
                self.save(context);
            }
        } else {
            self.dirty = false;
            self.say(
                BannerLevel::Error,
                "Your place in your books was not saved. Free some space on the reader.",
            );
        }
    }

    fn save(&mut self, context: &mut Context) {
        if self.load != Load::Ready || !self.shelf.fits() {
            return;
        }
        if self.write.is_some() {
            self.dirty = true;
            return;
        }
        let bytes = self.shelf.encode();
        context.store().save(books::KEY, bytes.clone());
        self.write = Some(bytes);
    }

    /// The answer to a shelf request. `true` if a transfer here wanted it.
    pub fn shelf(&mut self, context: &mut Context, name: &str, result: &StoreResult) -> bool {
        match self.view.shelf(context, name, result) {
            ShelfStep::Elsewhere => {}
            ShelfStep::Busy => return false,
            ShelfStep::Opened => {
                if let Job::OpeningShelf(started) = std::mem::take(&mut self.job) {
                    step_taken(context, "open", started, 0);
                }
                self.opened = true;
                self.notice = None;
                return true;
            }
            ShelfStep::Failed(fault) => {
                if let Job::OpeningShelf(started) = std::mem::take(&mut self.job) {
                    step_taken(context, "open", started, 0);
                }
                if let Some(book) = &self.current {
                    context.log(
                        LogLevel::Info,
                        format!(
                            "pharos: book open refused id={} from=shelf why={fault:?}",
                            book.id
                        ),
                    );
                }
                self.shelf_failed(&fault);
                return true;
            }
        }
        match std::mem::take(&mut self.job) {
            Job::Writing { received, last } => match result {
                StoreResult::ShelfWritten { name, size }
                    if Some(name) == self.current_file().as_ref() =>
                {
                    if usize::try_from(*size).ok() != Some(received) {
                        self.abandon_download(context);
                        self.say(
                            BannerLevel::Error,
                            "The book was not saved whole. Download it again.",
                        );
                    } else if last {
                        self.stored(context, received);
                    } else {
                        self.job = Job::Downloading {
                            task: None,
                            received,
                        };
                        self.fetch_piece(context);
                    }
                }
                StoreResult::Denied(_) => {
                    self.abandon_download(context);
                    self.say(
                        BannerLevel::Error,
                        "The book was not saved. Free some space and download it again.",
                    );
                }
                _ => {
                    self.job = Job::Writing { received, last };
                    return false;
                }
            },
            Job::Opening(mut local) => match local.advance(context, result) {
                ShelfProgress::Done => self.read_local(context, &local.take()),
                ShelfProgress::Failed(_) => self.damaged_copy(
                    "The downloaded book could not be loaded. Try opening it again.",
                    false,
                ),
                ShelfProgress::Moving { .. } => self.job = Job::Opening(local),
                ShelfProgress::Elsewhere => {
                    self.job = Job::Opening(local);
                    return false;
                }
            },
            Job::Removing(file) => match result {
                StoreResult::ShelfRemoved { name } if *name == file => {
                    self.shelf.remove(&file);
                    self.mode = Mode::List;
                    self.home_book = None;
                    self.say(BannerLevel::Success, "Download removed.");
                    self.save(context);
                }
                StoreResult::Denied(_) => {
                    self.say(BannerLevel::Error, "The download could not be removed.");
                }
                _ => {
                    self.job = Job::Removing(file);
                    return false;
                }
            },
            other => {
                self.job = other;
                return false;
            }
        }
        true
    }

    /// The file is on the shelf: record it and show it.
    fn stored(&mut self, context: &mut Context, size: usize) {
        let (Some(id), Some(file)) = (
            self.current.as_ref().map(|book| book.id.clone()),
            self.current.as_ref().and_then(Book::file),
        ) else {
            self.view.close(context);
            return;
        };
        let saved = Saved {
            id,
            file: file.clone(),
            size,
            memory: String::new(),
        };
        if self.shelf.insert(saved) {
            self.damaged = None;
            self.notice = None;
            self.save(context);
            self.open_saved(context, &file);
        } else {
            context.shelf().remove(file);
            self.say(
                BannerLevel::Error,
                "You have 64 downloaded books. Remove one first.",
            );
        }
    }

    fn read_local(&mut self, context: &mut Context, bytes: &[u8]) {
        let (Some(book), Some(saved)) = (
            self.current.clone(),
            self.current
                .as_ref()
                .and_then(|book| self.shelf.get(&book.id).cloned()),
        ) else {
            return;
        };
        let Support::Reads(kind) = book.support else {
            return;
        };
        let whole = bytes.len() == saved.size && books::is_what_it_claims(kind, bytes);
        let mut why = "not whole or not what it claims".to_owned();
        let opening = Instant::now();
        let opened = whole && {
            let result = self.view.open_bytes(
                context,
                &saved.file,
                bytes,
                Memory::decode(saved.memory.as_bytes()),
            );
            step_taken(context, "open", opening, bytes.len());
            match result {
                Ok(()) => true,
                Err(error) => {
                    why = error.to_string();
                    false
                }
            }
        };
        if opened {
            self.opened = true;
            self.notice = None;
        } else {
            refused(context, &book.id, kind, bytes, &why);
            self.damaged_copy(
                "The downloaded book is incomplete or unreadable. Open it again to download it.",
                true,
            );
        }
    }

    fn shelf_failed(&mut self, fault: &ShelfFault) {
        match fault {
            ShelfFault::Store(_) => self.damaged_copy(
                "The downloaded book could not be loaded. Try opening it again.",
                false,
            ),
            ShelfFault::Book(_) => self.damaged_copy(
                "The downloaded book is incomplete or unreadable. Open it again to download it.",
                true,
            ),
            ShelfFault::TooLarge { .. } => {
                self.damaged_copy("This book has more text than the reader can hold.", false);
            }
        }
    }

    fn damaged_copy(&mut self, why: &str, redownload: bool) {
        self.damaged = redownload
            .then(|| self.current.as_ref().map(|book| book.id.clone()))
            .flatten();
        self.say(BannerLevel::Error, why);
    }

    /// A task finished. `None` when it was not one of these screens'.
    pub fn task(
        &mut self,
        context: &mut Context,
        id: TaskId,
        outcome: &TaskOutcome,
    ) -> Option<Leave> {
        if self.subjects_task == Some(id) {
            self.subjects_task = None;
            match outcome {
                TaskOutcome::Completed(bytes) => match filters::parse_subjects(bytes) {
                    Some(subjects) => self.subjects = Some(subjects),
                    None => self.subjects_failed = true,
                },
                TaskOutcome::Failed(TaskError::Unauthorized) => return Some(Leave::SignOut),
                TaskOutcome::Failed(_) | TaskOutcome::Cancelled => self.subjects_failed = true,
            }
            return Some(Leave::Stay);
        }
        if self.known_task == Some(id) {
            self.known_task = None;
            match outcome {
                TaskOutcome::Completed(bytes) => {
                    if let Some(page) = books::parse_page(bytes) {
                        self.known = page.books;
                    }
                }
                TaskOutcome::Failed(TaskError::Unauthorized) => return Some(Leave::SignOut),
                // The home screen does without them until the next try.
                TaskOutcome::Failed(_) | TaskOutcome::Cancelled => {}
            }
            return Some(Leave::Stay);
        }
        if matches!(self.opening, Opening::Loading(task) if task == id) {
            return Some(self.opening_arrived(outcome));
        }
        if let Some((task, start)) = self.list_task {
            if task == id {
                self.list_task = None;
                let before = if start == 0 {
                    0
                } else {
                    self.list.entries.len()
                };
                let leave = self.listed(start, outcome);
                // The whole list, so every letter is there to jump to; a
                // block that added nothing ends it rather than asking again.
                let loaded = self.list.entries.len();
                if leave == Leave::Stay && loaded > before && loaded < self.list.total {
                    self.load_list(context, loaded);
                }
                return Some(leave);
            }
        }
        if let Job::Downloading { task, .. } = &self.job {
            if *task == Some(id) {
                return Some(self.piece(context, outcome));
            }
        }
        (self.view.woke(context, id, outcome) != Step::Elsewhere).then_some(Leave::Stay)
    }

    fn listed(&mut self, start: usize, outcome: &TaskOutcome) -> Leave {
        match outcome {
            TaskOutcome::Completed(bytes) => match books::parse_page(bytes) {
                Some(page) => {
                    if start == 0 {
                        self.list.restart();
                    }
                    let sent = page.books.len();
                    self.list
                        .take_block(start, sent, page.total, page.books, usize::MAX);
                }
                None => {
                    self.list_failed = Some("Pharos sent a list this app could not read.".into());
                }
            },
            TaskOutcome::Failed(TaskError::Unauthorized) => return Leave::SignOut,
            TaskOutcome::Failed(error) => {
                // A later block failing in the background keeps what loaded.
                if start == 0 {
                    self.list_failed = Some(task_words(*error).to_owned());
                }
            }
            TaskOutcome::Cancelled => {}
        }
        Leave::Stay
    }

    fn piece(&mut self, context: &mut Context, outcome: &TaskOutcome) -> Leave {
        let started = Instant::now();
        let Job::Downloading { received, .. } = std::mem::take(&mut self.job) else {
            return Leave::Stay;
        };
        let held = received;
        match outcome {
            TaskOutcome::Completed(piece) => match books::after_piece(
                self.current_kind().unwrap_or(Kind::Text),
                received,
                piece.len(),
            ) {
                Piece::TooBig => {
                    self.abandon_download(context);
                    let kind = self.current_kind().unwrap_or(Kind::Text);
                    self.say(BannerLevel::Error, kind.too_big());
                }
                next => self.write_piece(context, received, piece, next == Piece::Whole),
            },
            TaskOutcome::Failed(TaskError::Unauthorized) => {
                self.abandon_download(context);
                return Leave::SignOut;
            }
            TaskOutcome::Failed(error) => {
                self.abandon_download(context);
                self.say(BannerLevel::Error, task_words(*error));
            }
            TaskOutcome::Cancelled => {
                self.abandon_download(context);
                self.say(BannerLevel::Info, "Download cancelled.");
            }
        }
        step_taken(context, "piece", started, held);
        Leave::Stay
    }

    /// Sends one fetched piece to the shelf, after checking that the first
    /// piece starts the way the promised format does.
    fn write_piece(&mut self, context: &mut Context, received: usize, piece: &[u8], last: bool) {
        let (Some(book), Some(file)) = (self.current.as_ref(), self.current_file()) else {
            return;
        };
        let Support::Reads(kind) = book.support else {
            return;
        };
        if received == 0 && !books::is_what_it_claims(kind, piece) {
            refused(context, &book.id, kind, piece, "not what it claims");
            let why = if book.kindle {
                "Pharos couldn't convert this Kindle book to an EPUB. It may be locked with DRM."
                    .to_owned()
            } else {
                format!(
                    "Pharos sent a file this reader can't open as {}.",
                    kind_words(kind)
                )
            };
            self.say(BannerLevel::Error, why);
            return;
        }
        let (Ok(offset), Some(after)) = (
            u32::try_from(received),
            received
                .checked_add(piece.len())
                .filter(|after| u32::try_from(*after).is_ok()),
        ) else {
            self.abandon_download(context);
            self.say(BannerLevel::Error, kind.too_big());
            return;
        };
        context.shelf().write(file, offset, piece.to_vec(), last);
        self.job = Job::Writing {
            received: after,
            last,
        };
    }

    /// Drops whatever part of the book reached the shelf, so a half-written
    /// copy does not hold space the next download needs.
    fn abandon_download(&mut self, context: &mut Context) {
        self.job = Job::Idle;
        if let Some(file) = self.current_file() {
            context.shelf().remove(file);
        }
    }

    fn current_file(&self) -> Option<String> {
        self.current.as_ref().and_then(Book::file)
    }

    fn current_kind(&self) -> Option<Kind> {
        match self.current.as_ref()?.support {
            Support::Reads(kind) => Some(kind),
            Support::Cannot(_) => None,
        }
    }

    /// Opens a saved book: an EPUB straight off the shelf, anything else
    /// read back whole first.
    fn open_saved(&mut self, context: &mut Context, file: &str) {
        if self.current_kind() == Some(Kind::Epub) {
            let memory = self
                .current
                .as_ref()
                .and_then(|book| self.shelf.get(&book.id))
                .map(|saved| Memory::decode(saved.memory.as_bytes()))
                .unwrap_or_default();
            self.view.open_shelf(context, file, memory);
            self.job = Job::OpeningShelf(Instant::now());
            return;
        }
        let mut local = ShelfDownload::new(file).at_most(books::MAX_BYTES);
        local.start(context);
        self.job = Job::Opening(local);
    }

    fn fetch_piece(&mut self, context: &mut Context) {
        let Job::Downloading { received, .. } = &self.job else {
            return;
        };
        let (Some(book), Ok(offset)) = (&self.current, u32::try_from(*received)) else {
            self.job = Job::Idle;
            return;
        };
        let task = context.spawn(Task::Fetch {
            url: books::download_url(self.server, &book.id),
            offset,
            max_bytes: books::CHUNK,
            credential: None,
            headers: self.headers(),
        });
        if task.is_none() {
            self.abandon_download(context);
            self.say(
                BannerLevel::Error,
                "The download could not start. Try again.",
            );
        } else if let Job::Downloading { task: slot, .. } = &mut self.job {
            *slot = task;
        }
    }

    /// A tap or a key. `Leave::Back` returns to the albums.
    pub fn action(&mut self, context: &mut Context, action: ActionId) -> Leave {
        if self.opened {
            self.act_in_book(context, action);
            if !self.opened && std::mem::take(&mut self.from_home) {
                return Leave::Back;
            }
            return Leave::Stay;
        }
        if action == action_id("books-cancel") {
            match std::mem::take(&mut self.job) {
                Job::Downloading { task, .. } => {
                    if let Some(task) = task {
                        context.cancel(task);
                    }
                    self.abandon_download(context);
                    self.say(BannerLevel::Info, "Download cancelled.");
                }
                Job::Writing { .. } => {
                    self.abandon_download(context);
                    self.say(BannerLevel::Info, "Download cancelled.");
                }
                other => self.job = other,
            }
            return Leave::Stay;
        }
        if self.busy() {
            return Leave::Stay;
        }
        if self.pick_letter(context, action) {
            return Leave::Stay;
        }
        if self.filter_action(context, action) {
            return Leave::Stay;
        }
        if action == ActionId::BACK {
            self.mode = match self.mode {
                Mode::List => return Leave::Back,
                Mode::Menu(_) if self.home_book.is_some() => {
                    self.home_book = None;
                    self.from_home = false;
                    self.drop_opening(context);
                    return Leave::Back;
                }
                Mode::Menu(_) | Mode::Filter | Mode::Subjects(_) => Mode::List,
                Mode::About { index, .. } => Mode::Menu(index),
                Mode::Opening { index, .. } => {
                    // A request still out is not wanted once the page is left.
                    if let Opening::Loading(task) = self.opening {
                        context.cancel(task);
                        self.opening = Opening::None;
                    }
                    Mode::Menu(index)
                }
            };
            if !matches!(self.mode, Mode::Menu(_)) {
                self.drop_opening(context);
            }
        } else if action == action_id("books-retry") {
            self.list_failed = None;
            self.load_list(context, 0);
        } else if action == action_id("books-reload") {
            self.load = Load::Loading;
            context.store().load(books::KEY);
        } else if action == action_id("books-page-next") {
            self.turn(context, true);
        } else if action == action_id("books-page-back") {
            self.turn(context, false);
        } else if action == action_id("book-read") {
            let started = Instant::now();
            if let Mode::Menu(index) = self.mode {
                match self.home_book.take() {
                    Some(book) => self.begin_book(context, book),
                    None => self.begin(context, index),
                }
            }
            step_taken(context, "read-tap", started, 0);
        } else if action == action_id("book-remove") {
            self.remove(context);
        } else if action == action_id("book-about") {
            if let Mode::Menu(index) = self.mode {
                self.mode = Mode::About { index, page: 0 };
            }
        } else if action == action_id("book-opening") {
            if let Mode::Menu(index) = self.mode {
                self.read_opening(context, index);
            }
        } else if action == action_id("book-about-next") || action == action_id("book-opening-next")
        {
            self.turn_pager(context, true);
        } else if action == action_id("book-about-back") || action == action_id("book-opening-back")
        {
            self.turn_pager(context, false);
        } else {
            self.row_action(context, action);
        }
        Leave::Stay
    }

    fn row_action(&mut self, context: &mut Context, action: ActionId) {
        for index in self.list.page_indices(context) {
            if action == action_id(&format!("book-{index}"))
                || action == action_id(&format!("book-menu-{index}"))
            {
                self.notice = None;
                self.mode = Mode::Menu(index);
            }
        }
    }

    fn turn(&mut self, context: &mut Context, forward: bool) {
        self.list.turn(context, forward);
        let per_page = self.list.per_page(context).max(1);
        // Fetch the next block before the reader reaches the end of this one.
        let loaded = self.list.entries.len();
        if loaded < self.list.total && (self.list.page_number() + 2) * per_page >= loaded {
            self.load_list(context, loaded);
        }
    }

    /// Turns the page of the description or of the opening.
    fn turn_pager(&mut self, context: &Context, forward: bool) {
        let (index, page, pages) = match self.mode {
            Mode::About { index, page } => {
                let Some(book) = self.detail_book(index) else {
                    return;
                };
                (index, page, about::page_count(context, &book.overview))
            }
            Mode::Opening { index, page } => {
                let Opening::Ready(text) = &self.opening else {
                    return;
                };
                (index, page, about::page_count(context, text))
            }
            _ => return,
        };
        let page = if forward {
            (page + 1).min(pages.saturating_sub(1))
        } else {
            page.saturating_sub(1)
        };
        self.mode = match self.mode {
            Mode::Opening { .. } => Mode::Opening { index, page },
            _ => Mode::About { index, page },
        };
    }

    /// Asks pharos for the opening of the book at `index`, and shows its
    /// pages when it comes. One already fetched for this page is shown again.
    fn read_opening(&mut self, context: &mut Context, index: usize) {
        self.notice = None;
        if matches!(self.opening, Opening::Ready(_)) {
            self.mode = Mode::Opening { index, page: 0 };
            return;
        }
        let Some(book) = self.detail_book(index) else {
            return;
        };
        let url = books::opening_url(self.server, &book.id);
        let task = context.spawn(Task::Fetch {
            url,
            offset: 0,
            max_bytes: MAX_OPENING,
            credential: None,
            headers: self.headers(),
        });
        match task {
            Some(task) => {
                self.opening = Opening::Loading(task);
                self.mode = Mode::Opening { index, page: 0 };
            }
            None => self.say(BannerLevel::Error, "The opening could not be requested."),
        }
    }

    /// Forgets the opening, cancelling a request still out.
    fn drop_opening(&mut self, context: &mut Context) {
        if let Opening::Loading(task) = std::mem::take(&mut self.opening) {
            context.cancel(task);
        }
    }

    /// The answer to asking for the opening. The text is kept for the
    /// pager; a failure returns to the detail page with the reason.
    fn opening_arrived(&mut self, outcome: &TaskOutcome) -> Leave {
        self.opening = Opening::None;
        let failure = match outcome {
            TaskOutcome::Completed(bytes) => {
                let text = String::from_utf8_lossy(bytes).into_owned();
                if text.trim().is_empty() {
                    Some((BannerLevel::Attention, NO_OPENING))
                } else {
                    self.opening = Opening::Ready(text);
                    None
                }
            }
            TaskOutcome::Failed(TaskError::Unauthorized) => return Leave::SignOut,
            TaskOutcome::Failed(TaskError::NotFound) => Some((BannerLevel::Attention, NO_OPENING)),
            TaskOutcome::Failed(error) => Some((BannerLevel::Error, task_words(*error))),
            TaskOutcome::Cancelled => return Leave::Stay,
        };
        if let Some((level, words)) = failure {
            if let Mode::Opening { index, .. } = self.mode {
                self.mode = Mode::Menu(index);
            }
            self.say(level, words);
        }
        Leave::Stay
    }

    /// Opens the book at `index`, downloading it first if it is not here.
    fn begin(&mut self, context: &mut Context, index: usize) {
        if let Some(book) = self.list.entries.get(index).cloned() {
            self.begin_book(context, book);
        }
    }

    /// Opens `book`, downloading it first if it is not here.
    fn begin_book(&mut self, context: &mut Context, book: Book) {
        self.drop_opening(context);
        self.mode = Mode::List;
        self.notice = None;
        if let Support::Cannot(why) = book.support {
            self.say(BannerLevel::Attention, why);
            return;
        }
        let Some(file) = book.file() else {
            self.say(
                BannerLevel::Error,
                "This book can't be stored on the reader.",
            );
            return;
        };
        match self.load {
            Load::Ready => {}
            Load::Blocked => {
                self.say(
                    BannerLevel::Error,
                    "Your downloaded books could not be read. Try again first.",
                );
                return;
            }
            Load::Loading | Load::Unloaded => {
                self.say(BannerLevel::Attention, "Still loading your downloads.");
                return;
            }
        }
        let known = self.shelf.get(&book.id).is_some();
        let here = known && self.damaged.as_deref() != Some(&book.id);
        self.current = Some(book);
        if here {
            self.open_saved(context, &file);
        } else if !known && self.shelf.saved.len() >= books::MAX_DOWNLOADED {
            self.say(
                BannerLevel::Error,
                "You have 64 downloaded books. Remove one first.",
            );
        } else {
            self.job = Job::Downloading {
                task: None,
                received: 0,
            };
            self.fetch_piece(context);
        }
    }

    fn remove(&mut self, context: &mut Context) {
        let Mode::Menu(index) = self.mode else {
            return;
        };
        let Some(saved) = self
            .detail_book(index)
            .and_then(|book| self.shelf.get(&book.id))
            .cloned()
        else {
            return;
        };
        context.shelf().remove(saved.file.clone());
        self.job = Job::Removing(saved.file);
    }

    fn act_in_book(&mut self, context: &mut Context, action: ActionId) {
        match self.view.act(context, action) {
            None if action == ActionId::BACK => self.close_book(context),
            Some(Outcome::Close) => self.close_book(context),
            Some(Outcome::Save) => self.keep_place(context),
            Some(Outcome::Light(level)) => {
                context.device().set_frontlight(level);
                self.keep_place(context);
            }
            _ => {}
        }
    }

    fn close_book(&mut self, context: &mut Context) {
        self.keep_place(context);
        self.view.close(context);
        self.opened = false;
    }

    fn keep_place(&mut self, context: &mut Context) {
        let Some(book) = &self.current else {
            return;
        };
        if let Some(memory) = self.view.memory() {
            let encoded = String::from_utf8(memory.encode()).unwrap_or_default();
            self.shelf.remember(&book.id, encoded);
            self.save(context);
        }
    }
}

/// Records how long a step took on the main thread, for the frame log.
fn step_taken(context: &mut Context, step: &str, started: Instant, bytes: usize) {
    let ms = started.elapsed().as_millis();
    context.log(
        LogLevel::Info,
        format!("pharos: book step={step} ms={ms} bytes={bytes}"),
    );
}

/// Records why a book's bytes were not opened: its id, size and claimed
/// format, the first bytes as hex, and the reason. Never a title or a name.
fn refused(context: &mut Context, id: &str, kind: Kind, bytes: &[u8], why: &str) {
    let head = bytes.iter().take(8).fold(String::new(), |mut hex, byte| {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    context.log(
        LogLevel::Info,
        format!(
            "pharos: book open refused id={id} bytes={} kind={} head={head} why={why}",
            bytes.len(),
            kind_words(kind),
        ),
    );
}

fn own(screen: ScreenBuilder) -> Screen {
    screen.build().with_own_back(true)
}

fn kind_words(kind: Kind) -> &'static str {
    match kind {
        Kind::Epub => "an EPUB",
        Kind::Text => "text",
        Kind::Markdown => "Markdown",
        Kind::Html => "a web page",
    }
}

/// What a failed request means to the person holding the reader.
fn task_words(error: TaskError) -> &'static str {
    match error {
        TaskError::Offline => "The reader is offline. Turn on Wi-Fi and try again.",
        TaskError::NotFound => "Pharos has no file for this book.",
        TaskError::TooLarge => books::TOO_BIG,
        _ => "Pharos could not be reached. Try again.",
    }
}
