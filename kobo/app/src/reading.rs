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
use crate::books::{self, Book, BookPage, Kind, Piece, Saved, Shelf, Support};
use crate::letters;
use crate::MAX_JSON;
use kobo_bookview::{BookView, Step};
use kobo_read::{Memory, Outcome};
use kobo_sdk::{
    action_id, ActionId, BannerLevel, Context, Glyph, Header, Screen, ScreenBuilder, ShelfDownload,
    ShelfProgress, ShelfUpload, StoreResult, Task, TaskError, TaskId, TaskOutcome, TilePicture,
    TileShape, TileState,
};

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

/// What the screen is showing, when no book is open.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    List,
    /// The menu of the book at this index in the list.
    Menu(usize),
    /// A page of the description of the book at this index.
    About { index: usize, page: usize },
}

/// The one piece of work that may be running.
#[derive(Default)]
enum Job {
    #[default]
    Idle,
    Downloading {
        task: Option<TaskId>,
        bytes: Vec<u8>,
    },
    Saving {
        upload: ShelfUpload,
        size: usize,
    },
    Opening(ShelfDownload),
    Removing(String),
}

#[allow(clippy::struct_excessive_bools)] // independent flags, each documented
pub struct Reading {
    server: &'static str,
    version: &'static str,
    account: Option<Account>,
    load: Load,
    shelf: Shelf,
    list: BookPage,
    list_task: Option<(TaskId, usize)>,
    list_failed: Option<String>,
    page: usize,
    /// Whether the letters to jump the list to are showing (V1442).
    picking_letter: bool,
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
            list: BookPage::default(),
            list_task: None,
            list_failed: None,
            page: 0,
            picking_letter: false,
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
        }
    }

    /// Forgets everything of the signed-in account.
    pub fn reset(&mut self, context: &mut Context) {
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
        if self.list.books.is_empty() {
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
            .books
            .iter()
            .chain(&self.known)
            .find(|book| book.id == id)
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
            .or_else(|| self.list.books.get(index))
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
        let task = context.spawn(Task::Fetch {
            url: books::list_url(self.server, account, start),
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

    /// The list split into the pages of covers this panel holds.
    fn pages(&self, context: &Context) -> Vec<Vec<usize>> {
        context.paginate_tiles(self.list.books.len(), TileShape::Portrait, false)
    }

    /// The books on the page showing, by their place in the list.
    fn page_books(&self, context: &Context) -> Vec<usize> {
        let pages = self.pages(context);
        let page = self.page.min(pages.len().saturating_sub(1));
        pages.get(page).cloned().unwrap_or_default()
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
            Mode::Menu(index) | Mode::About { index, .. } => self.detail_book(index),
        };
        let pages = self.pages(context);
        let page = self.page.min(pages.len().saturating_sub(1));
        let listed = api::wanted(&pages, page)
            .into_iter()
            .filter_map(|index| self.list.books.get(index))
            .filter(|book| detail.is_none_or(|detail| detail.id != book.id));
        detail
            .into_iter()
            .chain(listed)
            .filter_map(|book| Some((book.id.clone(), book.image_tag.clone()?)))
            .collect()
    }

    /// The covers on the page showing that are tagged, for the app to tell
    /// whether all of them are in yet.
    #[must_use]
    pub fn page_covers(&self, context: &Context) -> Vec<String> {
        if self.opened || self.mode != Mode::List || self.busy() {
            return Vec::new();
        }
        self.page_books(context)
            .into_iter()
            .filter_map(|index| self.list.books.get(index))
            .filter(|book| book.image_tag.is_some())
            .map(|book| book.id.clone())
            .collect()
    }

    /// How many covers one page holds.
    #[must_use]
    pub fn per_page(&self, context: &Context) -> usize {
        self.pages(context).first().map_or(0, Vec::len)
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
        self.page_books(context)
            .into_iter()
            .filter_map(|index| {
                let book = self.list.books.get(index)?;
                let downloaded = self.downloaded(book);
                let subtitle = match book.support {
                    Support::Cannot(_) => "Can't open here".to_owned(),
                    Support::Reads(_) => book.author.clone(),
                };
                let (menu, state) = if downloaded {
                    (format!("book-menu-{index}"), TileState::Held)
                } else {
                    (String::new(), TileState::Normal)
                };
                Some((
                    format!("book-{index}"),
                    book.title.clone(),
                    subtitle,
                    Glyph::Book,
                    menu,
                    cover(&book.id),
                    state,
                ))
            })
            .collect()
    }

    /// Where each letter starts in the list (V1442).
    fn letter_starts(&self) -> Vec<(char, usize)> {
        letters::starts(self.list.books.iter().map(|book| book.title.as_str()))
    }

    /// The letters the list holds, over it while the reader picks one.
    fn letter_picker(&self, context: &Context, screen: ScreenBuilder) -> ScreenBuilder {
        if !self.picking_letter {
            return screen;
        }
        let showing = self
            .page_books(context)
            .first()
            .and_then(|&index| self.list.books.get(index))
            .map(|book| letters::letter_of(&book.title));
        letters::picker(screen, &self.letter_starts(), showing)
    }

    /// Answers the letter picker: a letter turns to the page its first book
    /// is on, and Back closes it. Whether the action was its.
    fn pick_letter(&mut self, context: &Context, action: ActionId) -> bool {
        if self.mode != Mode::List {
            return false;
        }
        if action == action_id("letters") {
            self.picking_letter = true;
            return true;
        }
        if !self.picking_letter {
            return false;
        }
        self.picking_letter = false;
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

    fn banner(&self, screen: ScreenBuilder) -> ScreenBuilder {
        match &self.notice {
            Some((level, text)) => screen.banner(*level, text),
            None => screen,
        }
    }

    fn busy_screen(&self) -> Option<Screen> {
        match &self.job {
            Job::Downloading { bytes, .. } => {
                let received = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                return Some(own(self
                    .banner(ScreenBuilder::new("pharos-book-busy").top_bar("Downloading"))
                    .transfer("Downloading the book", received, None)
                    .button("books-cancel", "Cancel")));
            }
            Job::Saving { .. } => {
                return Some(own(ScreenBuilder::new("pharos-book-busy")
                    .top_bar("Downloading")
                    .activity("Saving the book", None)));
            }
            Job::Opening(_) => {
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
        if let Some(why) = &self.list_failed {
            return own(ScreenBuilder::new("pharos-books")
                .top_bar("Books")
                .error_state(why.clone())
                .primary_button("books-retry", "Try again"));
        }
        if self.list.books.is_empty() {
            let screen = self.banner(ScreenBuilder::new("pharos-books").top_bar("Books"));
            return own(if self.list_task.is_some() {
                screen.activity("Loading your books", None)
            } else {
                screen.text("Pharos has no books for this account.")
            });
        }
        let mut screen = self.banner(
            ScreenBuilder::new("pharos-books")
                .top_bar(format!("Books · {}", self.list.total))
                .top_bar_action("letters", "A–Z"),
        );
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
            .compose(|screen| self.letter_picker(context, screen)))
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
    pub fn shelf(&mut self, context: &mut Context, result: &StoreResult) -> bool {
        match std::mem::take(&mut self.job) {
            Job::Saving { mut upload, size } => match upload.advance(context, result) {
                ShelfProgress::Done => self.stored(context, size),
                ShelfProgress::Failed(_) => {
                    self.view.close(context);
                    self.say(
                        BannerLevel::Error,
                        "The book was not saved. Free some space and download it again.",
                    );
                }
                ShelfProgress::Moving { .. } => self.job = Job::Saving { upload, size },
                ShelfProgress::Elsewhere => {
                    self.job = Job::Saving { upload, size };
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
            file,
            size,
            memory: String::new(),
        };
        if self.shelf.insert(saved) {
            self.damaged = None;
            self.opened = true;
            self.notice = None;
            self.save(context);
        } else {
            self.view.close(context);
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
        if whole
            && self
                .view
                .open_bytes(
                    context,
                    &saved.file,
                    bytes,
                    Memory::decode(saved.memory.as_bytes()),
                )
                .is_ok()
        {
            self.opened = true;
            self.notice = None;
        } else {
            self.damaged_copy(
                "The downloaded book is incomplete or unreadable. Open it again to download it.",
                true,
            );
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
        if let Some((task, start)) = self.list_task {
            if task == id {
                self.list_task = None;
                let before = if start == 0 { 0 } else { self.list.books.len() };
                let leave = self.listed(start, outcome);
                // The whole list, so every letter is there to jump to; a
                // block that added nothing ends it rather than asking again.
                let loaded = self.list.books.len();
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
                        self.list = page;
                        self.page = 0;
                    } else if start == self.list.books.len() {
                        self.list.books.extend(page.books);
                        self.list.total = page.total;
                    }
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
        let Job::Downloading { bytes, .. } = std::mem::take(&mut self.job) else {
            return Leave::Stay;
        };
        match outcome {
            TaskOutcome::Completed(piece) => match books::after_piece(bytes.len(), piece.len()) {
                Piece::TooBig => {
                    self.say(BannerLevel::Error, books::TOO_BIG);
                }
                next => {
                    let mut bytes = bytes;
                    bytes.extend_from_slice(piece);
                    if next == Piece::More {
                        self.job = Job::Downloading { task: None, bytes };
                        self.fetch_piece(context);
                    } else {
                        self.finish_download(context, bytes);
                    }
                }
            },
            TaskOutcome::Failed(TaskError::Unauthorized) => return Leave::SignOut,
            TaskOutcome::Failed(error) => self.say(BannerLevel::Error, task_words(*error)),
            TaskOutcome::Cancelled => self.say(BannerLevel::Info, "Download cancelled."),
        }
        Leave::Stay
    }

    fn fetch_piece(&mut self, context: &mut Context) {
        let Job::Downloading { bytes, .. } = &self.job else {
            return;
        };
        let (Some(book), Ok(offset)) = (&self.current, u32::try_from(bytes.len())) else {
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
            self.job = Job::Idle;
            self.say(
                BannerLevel::Error,
                "The download could not start. Try again.",
            );
        } else if let Job::Downloading { task: slot, .. } = &mut self.job {
            *slot = task;
        }
    }

    fn finish_download(&mut self, context: &mut Context, bytes: Vec<u8>) {
        let (Some(book), Some(file)) = (
            self.current.as_ref(),
            self.current.as_ref().and_then(Book::file),
        ) else {
            return;
        };
        let Support::Reads(kind) = book.support else {
            return;
        };
        if !books::is_what_it_claims(kind, &bytes) {
            self.say(
                BannerLevel::Error,
                format!(
                    "Pharos sent a file this reader can't open as {}.",
                    kind_words(kind)
                ),
            );
            return;
        }
        if self
            .view
            .open_bytes(context, &file, &bytes, Memory::default())
            .is_err()
        {
            self.say(
                BannerLevel::Error,
                "This download is not a readable book. Check the file on Pharos.",
            );
            return;
        }
        let size = bytes.len();
        let mut upload = ShelfUpload::new(&file, bytes);
        upload.start(context);
        self.job = Job::Saving { upload, size };
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
            if let Job::Downloading { task, .. } = std::mem::take(&mut self.job) {
                if let Some(task) = task {
                    context.cancel(task);
                }
                self.say(BannerLevel::Info, "Download cancelled.");
            }
            return Leave::Stay;
        }
        if self.busy() {
            return Leave::Stay;
        }
        if self.pick_letter(context, action) {
            return Leave::Stay;
        }
        if action == ActionId::BACK {
            self.mode = match self.mode {
                Mode::List => return Leave::Back,
                Mode::Menu(_) if self.home_book.is_some() => {
                    self.home_book = None;
                    self.from_home = false;
                    return Leave::Back;
                }
                Mode::Menu(_) => Mode::List,
                Mode::About { index, .. } => Mode::Menu(index),
            };
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
            if let Mode::Menu(index) = self.mode {
                match self.home_book.take() {
                    Some(book) => self.begin_book(context, book),
                    None => self.begin(context, index),
                }
            }
        } else if action == action_id("book-remove") {
            self.remove(context);
        } else if action == action_id("book-about") {
            if let Mode::Menu(index) = self.mode {
                self.mode = Mode::About { index, page: 0 };
            }
        } else if action == action_id("book-about-next") {
            self.turn_about(context, true);
        } else if action == action_id("book-about-back") {
            self.turn_about(context, false);
        } else {
            self.row_action(context, action);
        }
        Leave::Stay
    }

    fn row_action(&mut self, context: &mut Context, action: ActionId) {
        for index in self.page_books(context) {
            if action == action_id(&format!("book-{index}"))
                || action == action_id(&format!("book-menu-{index}"))
            {
                self.notice = None;
                self.mode = Mode::Menu(index);
            }
        }
    }

    fn turn(&mut self, context: &mut Context, forward: bool) {
        let pages = self.pages(context);
        let per_page = pages.first().map_or(1, Vec::len).max(1);
        let pages = pages.len().max(1);
        if forward && self.page + 1 < pages {
            self.page += 1;
        } else if !forward && self.page > 0 {
            self.page -= 1;
        }
        // Fetch the next block before the reader reaches the end of this one.
        let loaded = self.list.books.len();
        if loaded < self.list.total && (self.page + 2) * per_page >= loaded {
            self.load_list(context, loaded);
        }
    }

    fn turn_about(&mut self, context: &Context, forward: bool) {
        let Mode::About { index, page } = self.mode else {
            return;
        };
        let Some(book) = self.detail_book(index) else {
            return;
        };
        let pages = about::page_count(context, &book.overview);
        let page = if forward {
            (page + 1).min(pages.saturating_sub(1))
        } else {
            page.saturating_sub(1)
        };
        self.mode = Mode::About { index, page };
    }

    /// Opens the book at `index`, downloading it first if it is not here.
    fn begin(&mut self, context: &mut Context, index: usize) {
        if let Some(book) = self.list.books.get(index).cloned() {
            self.begin_book(context, book);
        }
    }

    /// Opens `book`, downloading it first if it is not here.
    fn begin_book(&mut self, context: &mut Context, book: Book) {
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
            let mut local = ShelfDownload::new(&file).at_most(books::MAX_BYTES);
            local.start(context);
            self.job = Job::Opening(local);
        } else if !known && self.shelf.saved.len() >= books::MAX_DOWNLOADED {
            self.say(
                BannerLevel::Error,
                "You have 64 downloaded books. Remove one first.",
            );
        } else {
            self.job = Job::Downloading {
                task: None,
                bytes: Vec::new(),
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
