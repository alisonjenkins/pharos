//! Pharos on a Kobo (spec 047, ADR-0136).
//!
//! Signs in to the owner's pharos with a Quick Connect code, shows a home
//! screen with what the owner was last listening to and reading (V1445) and a
//! section for each kind of library the owner can see, lists
//! the albums and audiobooks in one, and plays a track through the runtime's Bluetooth audio, telling
//! pharos where the listener is so any other device can carry on.
//!
//! The server address is fixed when the package is built
//! (`PHAROS_KOBO_SERVER`); the reader has one owner and one server.

mod about;
mod api;
mod books;
mod letters;
mod reading;

use api::{Account, Cover, CoverCache, Entry, Kind, Page, Report, Resumed, Section};
use kobo_sdk::audio::{AudioMetadata, AudioPlayer};
use kobo_sdk::prelude::*;
use kobo_sdk::{
    Glyph, Header, LogLevel, PictureHandle, Position, Task, TaskError, TaskId, TaskOutcome,
    TilePicture, TileShape, TileState, MAX_TASKS_IN_FLIGHT,
};
use reading::{Leave, Reading};
use std::process::ExitCode;

const SERVER: &str = match option_env!("PHAROS_KOBO_SERVER") {
    Some(server) => server,
    None => "https://pharos.test",
};
const VERSION: &str = env!("CARGO_PKG_VERSION");
const ACCOUNT_KEY: &str = "account";
const POLL_SECONDS: u32 = 3;
/// How often a playing track's place is sent while nobody touches anything.
const REPORT_SECONDS: u32 = 30;
/// How long a screen waits for what it shows to arrive before it is drawn
/// with what has, so a slow server still shows something.
const DRAW_WAIT_SECONDS: u32 = 2;
const MAX_JSON: u32 = 512 * 1024;
/// How wide the cover is on an album's masthead, in millimetres: about what
/// a tile is on the smallest reader, so the picture already held fits.
const HERO_COVER_MM: u16 = 26;
/// The most rows of a shelf that are fetched. A shelf loads whole, in blocks
/// of [`api::PAGE`], because pharos cannot filter audiobooks from music on
/// its side; this stops a huge library from being read forever.
const MAX_SHELF: usize = 2_000;
/// The largest cover reply taken. A cover is sized to a tile, so this is
/// many times what one needs.
const MAX_COVER: u32 = 1024 * 1024;
/// How many covers are fetched at once. The runtime runs four tasks in all,
/// and the sign-in poll, the playing report and the player need the rest.
const MAX_COVER_TASKS: usize = 2;
/// The fewest covers held. The runtime keeps every picture until told
/// otherwise, so the held covers are bounded to a few pages of tiles: at
/// least this, or three pages when a panel shows more than a few tiles.
const MIN_COVERS: usize = 24;
/// What the home screen's covers are held under, ahead of the item's id. Its
/// tiles are square, as small as a tile comes, so both recent rows and the
/// libraries share one panel; held apart, a cover sized for them never
/// stands in on a shelf or the book grid, whose tiles are bigger.
const HOME_COVER: &str = "home/";

#[derive(Debug)]
enum View {
    /// Reading the saved account, before anything is known.
    Opening,
    /// Asking for, or showing, a Quick Connect code.
    SignIn {
        code: Option<String>,
    },
    /// What the owner was last listening to and reading, then the sections:
    /// one for each kind of library the owner can see.
    Home,
    /// A shelf of albums or audiobooks.
    Library(Kind),
    /// The ebook list and the reader.
    Books,
    Album(Entry),
    Player,
    Problem(String),
}

/// What a network task was for.
#[derive(Clone, Debug)]
enum Work {
    Initiate,
    Wait {
        secret: String,
    },
    Poll {
        secret: String,
    },
    Finish,
    Views,
    /// The part-played tracks, for the home screen's Continue listening row.
    Listening,
    /// Pharos's description of one album in that row.
    ListeningAlbum {
        id: String,
    },
    /// One block of a shelf's albums, from `start` rows into the listing.
    Shelf {
        kind: Kind,
        start: usize,
    },
    /// The cover of one album.
    Cover {
        id: String,
    },
    /// One block of an album's tracks, from `start`.
    Tracks {
        album: String,
        start: usize,
    },
    Report(Report),
    ReportTimer,
    /// A screen's wait for what it shows, ended by the first draw.
    DrawWait,
}

struct Playing {
    player: AudioPlayer,
    /// The album the track belongs to, whose cover the player shows.
    album: Entry,
    track: String,
    sound_reported: bool,
}

#[allow(clippy::struct_excessive_bools)] // independent flags, each documented
struct Pharos {
    view: View,
    account: Option<Account>,
    tasks: Vec<(TaskId, Work)>,
    /// The sections, once pharos has said which libraries there are.
    sections: Option<Vec<Section>>,
    /// The kind of album the shelf holds, which is the one last opened.
    kind: Kind,
    /// The albums of that kind loaded so far.
    shelf: Page,
    /// How many rows of the listing have been asked for, kept or not.
    shelf_sent: usize,
    /// Whether the whole listing has been read.
    shelf_done: bool,
    /// How many albums the shelf's top bar counts. It catches up when the
    /// screen is drawn for something else, so a later block of albums
    /// landing behind the page showing draws nothing of its own.
    shelf_counted: usize,
    shelf_page: usize,
    /// Whether the letters to jump the shelf to are showing (V1442).
    picking_letter: bool,
    covers: CoverCache,
    /// The screen last handed to the runtime, so a task that changes nothing
    /// on it does not make the runtime draw the whole panel again.
    shown: Option<Screen>,
    tracks: Page,
    page: usize,
    /// The album whose tracks are loaded, for Back from the player.
    album: Option<Entry>,
    /// The page of the open album's description on show in place of its
    /// tracks, if it is.
    about: Option<usize>,
    playing: Option<Playing>,
    report_timer: Option<TaskId>,
    /// The [`Work::DrawWait`] running, which holds one of the runtime's few
    /// task slots until it is cancelled.
    draw_wait: Option<TaskId>,
    reading: Reading,
    /// The albums and audiobooks the owner stopped part-way through, newest
    /// first (V1445).
    listening: Vec<Resumed>,
    /// The track to carry on from, and where, once the open album's tracks
    /// arrive: set when the album was opened from Continue listening.
    resume_at: Option<(String, u64)>,
    /// Whether that track plays as soon as it has loaded: a hold on the tile
    /// or the Resume button, not the tap that only opened the album.
    resume_now: bool,
    /// Whether the open album came from the home screen, which Back returns to.
    album_from_home: bool,
}

impl Default for Pharos {
    fn default() -> Self {
        Self {
            view: View::Opening,
            account: None,
            tasks: Vec::new(),
            sections: None,
            kind: Kind::Music,
            shelf: Page::default(),
            shelf_sent: 0,
            shelf_done: false,
            shelf_counted: 0,
            shelf_page: 0,
            picking_letter: false,
            covers: CoverCache::new(MIN_COVERS),
            shown: None,
            tracks: Page::default(),
            page: 0,
            album: None,
            about: None,
            playing: None,
            report_timer: None,
            draw_wait: None,
            reading: Reading::new(SERVER, VERSION),
            listening: Vec::new(),
            resume_at: None,
            resume_now: false,
            album_from_home: false,
        }
    }
}

impl Pharos {
    fn header(&self) -> Vec<Header> {
        let token = self.account.as_ref().map(|account| account.token.as_str());
        vec![Header::new(
            "X-Emby-Authorization",
            api::client_header_value(VERSION, token),
        )]
    }

    fn track(&mut self, context: &mut Context, task: Option<TaskId>, work: Work) {
        if let Some(task) = task {
            self.tasks.push((task, work));
        } else {
            context.log(
                LogLevel::Info,
                format!("pharos: task not queued: {}", work_label(&work)),
            );
        }
    }

    fn fetch(&mut self, context: &mut Context, url: String, work: Work) {
        let task = context.spawn(Task::Fetch {
            url,
            offset: 0,
            max_bytes: MAX_JSON,
            credential: None,
            headers: self.header(),
        });
        self.track(context, task, work);
    }

    fn post(&mut self, context: &mut Context, url: String, body: String, work: Work) {
        let task = context.spawn(Task::Post {
            url,
            body,
            content_type: "application/json".to_owned(),
            credential: None,
            headers: self.header(),
            max_bytes: MAX_JSON,
        });
        self.track(context, task, work);
    }

    fn sleep(&mut self, context: &mut Context, seconds: u32, work: Work) -> Option<TaskId> {
        let task = context.spawn(Task::Sleep { seconds });
        self.track(context, task, work);
        task
    }

    fn sign_in(&mut self, context: &mut Context) {
        self.view = View::SignIn { code: None };
        self.post(
            context,
            api::initiate_url(SERVER),
            String::new(),
            Work::Initiate,
        );
    }

    fn sign_out(&mut self, context: &mut Context) {
        self.stop_playing(context);
        self.account = None;
        self.sections = None;
        self.listening.clear();
        self.resume_at = None;
        self.resume_now = false;
        self.album_from_home = false;
        self.clear_shelf();
        self.drop_covers(context);
        self.reading.reset(context);
        context.store().forget(ACCOUNT_KEY);
        self.sign_in(context);
    }

    fn load_views(&mut self, context: &mut Context) {
        let Some(account) = &self.account else {
            return;
        };
        let url = api::views_url(SERVER, account);
        let account = account.clone();
        self.view = View::Home;
        self.sections = None;
        self.fetch(context, url, Work::Views);
        self.reading.prepare(context, &account);
        self.go_home(context);
    }

    /// Shows the home screen, asking pharos again what was played last, so
    /// a track listened to here, or anywhere else, comes first.
    fn go_home(&mut self, context: &mut Context) {
        self.view = View::Home;
        let Some(account) = &self.account else {
            return;
        };
        if self
            .tasks
            .iter()
            .any(|(_, work)| matches!(work, Work::Listening))
        {
            return;
        }
        let url = api::resume_audio_url(SERVER, account);
        self.fetch(context, url, Work::Listening);
        self.reading.fetch_known(context);
    }

    /// How many tiles one row of the home screen holds.
    fn home_columns(context: &Context) -> usize {
        let metrics = context.metrics();
        let margin = metrics.screen_margin();
        metrics
            .grid_columns_for_width(TileShape::Square, metrics.width - 2 * margin)
            .max(1)
    }

    /// Takes the part-played albums. An album already described keeps its
    /// description; the rest on show are described one at a time.
    fn listening_arrived(&mut self, context: &mut Context, bytes: &[u8]) {
        let Some(mut resumed) = api::parse_resumed(bytes) else {
            context.log(LogLevel::Info, "pharos: resume list unreadable");
            return;
        };
        for fresh in &mut resumed {
            if let Some(known) = self
                .listening
                .iter()
                .find(|known| known.described && known.album.id == fresh.album.id)
            {
                fresh.album = known.album.clone();
                fresh.described = true;
            }
        }
        context.log(
            LogLevel::Info,
            format!("pharos: home listening albums={}", resumed.len()),
        );
        self.listening = resumed;
        self.describe_next(context);
    }

    /// Asks pharos about the albums on the home screen that only their
    /// tracks have named, for each one's cover and whether it is an
    /// audiobook: two at a time, so the row settles in one round rather
    /// than one album after another, and covers still find a free slot.
    fn describe_next(&mut self, context: &mut Context) {
        const AT_ONCE: usize = 2;
        let Some(account) = self.account.clone() else {
            return;
        };
        let asking: Vec<String> = self
            .tasks
            .iter()
            .filter_map(|(_, work)| match work {
                Work::ListeningAlbum { id } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let next: Vec<String> = self
            .listening
            .iter()
            .take(Self::home_columns(context))
            .filter(|resumed| !resumed.described && !asking.contains(&resumed.album.id))
            .map(|resumed| resumed.album.id.clone())
            .take(AT_ONCE.saturating_sub(asking.len()))
            .collect();
        for id in next {
            let url = api::item_url(SERVER, &account, &id);
            self.fetch(context, url, Work::ListeningAlbum { id });
        }
    }

    /// Settles one album of the row: described when pharos answered, as the
    /// track named it when it did not.
    fn album_described(&mut self, context: &mut Context, id: &str, album: Option<Entry>) {
        if let Some(resumed) = self
            .listening
            .iter_mut()
            .find(|resumed| resumed.album.id == id)
        {
            if let Some(album) = album.filter(|album| album.id == id) {
                resumed.album = album;
            }
            resumed.described = true;
        }
        self.describe_next(context);
    }

    /// Lets go of every cover and stops the ones on their way.
    fn drop_covers(&mut self, context: &mut Context) {
        let covering: Vec<TaskId> = self
            .tasks
            .iter()
            .filter(|(_, work)| matches!(work, Work::Cover { .. }))
            .map(|(task, _)| *task)
            .collect();
        for task in covering {
            context.cancel(task);
            self.tasks.retain(|(id, _)| *id != task);
        }
        for handle in self.covers.clear() {
            context.drop_picture(PictureHandle(handle));
        }
    }

    /// The tile shape a kind's shelf is drawn in.
    fn shape(kind: Kind) -> TileShape {
        if kind.portrait() {
            TileShape::Portrait
        } else {
            TileShape::Square
        }
    }

    /// The tiles of the shelf, split into the pages this panel holds.
    fn grid_pages(&self, context: &Context) -> Vec<Vec<usize>> {
        context.paginate_tiles(self.shelf.entries.len(), Self::shape(self.kind), false)
    }

    /// The pixel size the cover held as `key` is asked for and fitted to:
    /// the tile it fills. A home screen cover fills a square tile wherever
    /// the reader has gone by the time it comes.
    fn cover_size(&self, context: &Context, key: &str) -> (u32, u32) {
        let metrics = context.metrics();
        let kind = if key.starts_with(HOME_COVER) {
            Kind::Music
        } else {
            self.cover_kind()
        };
        let shape = Self::shape(kind);
        let margin = metrics.screen_margin();
        let columns = metrics.grid_columns_for_width(shape, metrics.width - 2 * margin);
        api::cover_px(
            metrics.width,
            margin,
            metrics.tenth_mm(20),
            columns,
            kind.portrait(),
        )
    }

    /// The kind whose tiles the covers on show fill: the ebook list's while
    /// it shows, else the shelf's.
    fn cover_kind(&self) -> Kind {
        if matches!(self.view, View::Books) {
            Kind::Books
        } else {
            self.kind
        }
    }

    /// The cover for `id`, once the runtime holds it.
    fn cover_of(&self, id: &str) -> Option<TilePicture> {
        match self.covers.state(id)? {
            Cover::Ready {
                handle,
                width,
                height,
            } => Some(TilePicture::new(PictureHandle(handle), width, height)),
            Cover::Loading | Cover::Failed => None,
        }
    }

    /// The covers worth having now: those on the page showing and the next
    /// one, as `(album id, cover tag)`. An album with no cover tag has none.
    fn wanted_covers(&self, context: &Context) -> (Vec<(String, String)>, usize) {
        let tagged = |entry: &Entry| Some((entry.id.clone(), entry.image_tag.clone()?));
        let shelf_covers = || {
            let pages = self.grid_pages(context);
            let page = self.shelf_page.min(pages.len().saturating_sub(1));
            let wanted: Vec<(String, String)> = api::wanted(&pages, page)
                .into_iter()
                .filter_map(|index| self.shelf.entries.get(index))
                .filter_map(tagged)
                .collect();
            (wanted, pages.first().map_or(0, Vec::len))
        };
        let (mut wanted, per_page) = match &self.view {
            // The shelf stays behind an open album, so its covers on the way
            // keep coming, after the album's own: Back finds them drawn
            // instead of asking for them again.
            View::Album(album) => {
                let (behind, per_page) = shelf_covers();
                let own = tagged(album);
                let wanted = own
                    .clone()
                    .into_iter()
                    .chain(
                        behind
                            .into_iter()
                            .filter(|cover| own.as_ref() != Some(cover)),
                    )
                    .collect();
                (wanted, per_page)
            }
            View::Library(_) => shelf_covers(),
            View::Home => {
                let wanted: Vec<(String, String)> = self
                    .home_covers(context)
                    .into_iter()
                    .filter_map(|(id, tag)| Some((id, tag?)))
                    .collect();
                let count = wanted.len();
                (wanted, count)
            }
            View::Books => (
                self.reading.wanted_covers(context),
                self.reading.per_page(context),
            ),
            _ => (Vec::new(), 0),
        };
        // The player keeps showing its cover whatever else is on screen.
        if let Some(cover) = self
            .playing
            .as_ref()
            .and_then(|playing| tagged(&playing.album))
        {
            if !wanted.contains(&cover) {
                wanted.push(cover);
            }
        }
        (wanted, per_page)
    }

    /// Brings the covers in line with what is showing: keeps those still
    /// wanted, stops those no longer wanted, and starts the next few.
    fn refresh_covers(&mut self, context: &mut Context) {
        let (wanted, per_page) = self.wanted_covers(context);
        self.covers.set_capacity(MIN_COVERS.max(per_page * 3));
        // What is on show and what is playing keep their covers.
        let pinned = self
            .album
            .iter()
            .chain(self.playing.iter().map(|playing| &playing.album))
            .map(|album| album.id.clone())
            .collect();
        self.covers.set_pinned(pinned);
        // Last wanted first, so the page showing is the newest and the last
        // to be pushed out.
        for (id, _) in wanted.iter().rev() {
            self.covers.touch(id);
        }
        let stale: Vec<(TaskId, String)> = self
            .tasks
            .iter()
            .filter_map(|(task, work)| match work {
                Work::Cover { id } if !wanted.iter().any(|(wanted, _)| wanted == id) => {
                    Some((*task, id.clone()))
                }
                _ => None,
            })
            .collect();
        for (task, id) in stale {
            context.cancel(task);
            self.tasks.retain(|(held, _)| *held != task);
            self.covers.release(&id);
        }
        let mut fetching = self
            .tasks
            .iter()
            .filter(|(_, work)| matches!(work, Work::Cover { .. }))
            .count();
        for (id, tag) in &wanted {
            if self.covers.state(id).is_some() {
                continue;
            }
            // Leave a task free for everything that is not a cover.
            if fetching >= MAX_COVER_TASKS || context.tasks_in_flight() + 1 >= MAX_TASKS_IN_FLIGHT {
                break;
            }
            let (width, height) = self.cover_size(context, id);
            let item = id.strip_prefix(HOME_COVER).unwrap_or(id);
            let url = api::image_url(SERVER, item, Some(tag), width, height);
            let Some(task) = context.spawn(Task::Fetch {
                url,
                offset: 0,
                max_bytes: MAX_COVER,
                credential: None,
                headers: self.header(),
            }) else {
                break;
            };
            self.tasks.push((task, Work::Cover { id: id.clone() }));
            for handle in self.covers.begin(id) {
                context.drop_picture(PictureHandle(handle));
            }
            fetching += 1;
        }
    }
    /// Whether the home screen is still waiting on pharos: for its
    /// libraries, what was played last, a description of an album in it, or
    /// a cover it shows.
    fn home_loading(&self, context: &Context) -> bool {
        self.tasks.iter().any(|(_, work)| {
            matches!(
                work,
                Work::Views | Work::Listening | Work::ListeningAlbum { .. }
            )
        }) || self.reading.recent_loading()
            || self.page_covers_pending(context)
    }

    /// Whether the screen showing still waits on what it shows: the home
    /// screen on its rows and covers, a shelf on its first albums and the
    /// covers of the page showing.
    fn screen_loading(&self, context: &Context) -> bool {
        match self.view {
            View::Home => self.home_loading(context),
            View::Library(_) => self.shelf_loading(context),
            _ => false,
        }
    }

    /// Whether the shelf has nothing to show yet but is asking, or a cover
    /// on the page showing is still to come.
    fn shelf_loading(&self, context: &Context) -> bool {
        let asking = self
            .tasks
            .iter()
            .any(|(_, work)| matches!(work, Work::Shelf { .. }));
        (asking && self.shelf.entries.is_empty()) || self.page_covers_pending(context)
    }

    /// Whether a cover on the shelf page showing is still to come: asked
    /// for and not here yet, or not asked for yet. A cover that failed is
    /// not waited for.
    fn page_covers_pending(&self, context: &Context) -> bool {
        if matches!(self.view, View::Books) {
            return self
                .reading
                .page_covers(context)
                .iter()
                .any(|id| matches!(self.covers.state(id), None | Some(Cover::Loading)));
        }
        if matches!(self.view, View::Home) {
            return self.home_covers(context).iter().any(|(id, tag)| {
                tag.is_some() && matches!(self.covers.state(id), None | Some(Cover::Loading))
            });
        }
        if !matches!(self.view, View::Library(_)) {
            return false;
        }
        let pages = self.grid_pages(context);
        let page = self.shelf_page.min(pages.len().saturating_sub(1));
        pages
            .get(page)
            .into_iter()
            .flatten()
            .filter_map(|&index| self.shelf.entries.get(index))
            .filter(|entry| entry.image_tag.is_some())
            .any(|entry| matches!(self.covers.state(&entry.id), None | Some(Cover::Loading)))
    }

    /// Takes a cover that arrived: fits it to its tile, and hands it to the
    /// runtime in colour only when the panel can show colour.
    fn cover_arrived(&mut self, context: &mut Context, id: &str, bytes: &[u8]) {
        if self.covers.state(id) != Some(Cover::Loading) {
            return;
        }
        let (width, height) = self.cover_size(context, id);
        let colour = context.panel_shows_colour();
        let decoded = if colour {
            kobo_image::decode_colour(bytes)
        } else {
            kobo_image::decode(bytes)
        };
        // Fitted whole, not cropped to the tile: most audiobook art is square,
        // and filling a tall tile with it cut a third off its sides. The
        // runtime centres a picture that is not the tile's shape.
        let picture = match decoded.and_then(|picture| {
            picture.prepare(width, height, kobo_image::FitMode::ContainEnlarging)
        }) {
            Ok(picture) => picture,
            Err(error) => return self.cover_failed(context, id, &error.to_string()),
        };
        let (width, height) = (picture.width(), picture.height());
        let rgb = if colour {
            picture.colour().map(<[u8]>::to_vec)
        } else {
            None
        };
        let Some(handle) = self.covers.ready(id, width, height) else {
            return;
        };
        let shown = context.put_picture_shown(
            PictureHandle(handle),
            width,
            height,
            picture.into_grey(),
            rgb,
        );
        if shown.is_none() {
            self.cover_failed(context, id, "the runtime would not take the picture");
            return;
        }
        // A track started before its cover arrived picks it up now.
        let cover = self.cover_of(id);
        if let Some(playing) = self
            .playing
            .as_mut()
            .filter(|playing| playing.album.id == id)
        {
            playing.player.set_cover(cover);
        }
    }

    /// A cover that cannot be had keeps its glyph. It is logged the first
    /// time only, and not asked for again while it stays held.
    fn cover_failed(&mut self, context: &mut Context, id: &str, why: &str) {
        if self.covers.failed(id) {
            context.log(LogLevel::Info, format!("pharos: cover {id} skipped: {why}"));
        }
    }

    /// Asks for the shelf's next block of albums, unless it has them all,
    /// one is on the way, or a page waits to be drawn: that wait holds a
    /// task slot, and the page's covers are wanted before the albums on
    /// pages not on show.
    fn more_shelf(&mut self, context: &mut Context) {
        let asking = self
            .tasks
            .iter()
            .any(|(_, work)| matches!(work, Work::Shelf { .. }));
        if self.shelf_done
            || asking
            || self.draw_wait.is_some()
            || !matches!(self.view, View::Library(_))
        {
            return;
        }
        if let Some(library) = self.shelf_library() {
            let next = self.shelf_sent;
            self.load_shelf(context, &library, next);
        }
    }

    fn clear_shelf(&mut self) {
        self.shelf = Page::default();
        self.shelf_sent = 0;
        self.shelf_done = false;
        self.shelf_counted = 0;
        self.shelf_page = 0;
        self.picking_letter = false;
    }

    /// Opens a section: albums load into a shelf, books open the ebook list.
    fn open_section(&mut self, context: &mut Context, kind: Kind) {
        let Some(library) = self
            .sections
            .iter()
            .flatten()
            .find(|section| section.kind == kind)
            .map(|section| section.library.clone())
        else {
            return;
        };
        if !kind.is_albums() {
            if let Some(account) = &self.account {
                self.reading.open(context, account);
                self.view = View::Books;
            }
            return;
        }
        self.clear_shelf();
        self.kind = kind;
        self.view = View::Library(kind);
        self.load_shelf(context, &library, 0);
    }

    fn load_shelf(&mut self, context: &mut Context, library: &str, start: usize) {
        let Some(account) = &self.account else {
            return;
        };
        let url = api::albums_url(SERVER, account, library, start);
        let kind = self.kind;
        self.fetch(context, url, Work::Shelf { kind, start });
    }

    /// The library the open shelf is listed from.
    fn shelf_library(&self) -> Option<String> {
        self.sections
            .iter()
            .flatten()
            .find(|section| section.kind == self.kind)
            .map(|section| section.library.clone())
    }

    fn open_album(&mut self, context: &mut Context, album: Entry) {
        if self.account.is_none() {
            return;
        }
        self.tracks = Page::default();
        self.page = 0;
        self.about = None;
        self.resume_at = None;
        self.resume_now = false;
        self.album_from_home = false;
        self.album = Some(album.clone());
        self.load_tracks(context, album.id.clone(), 0);
        self.view = View::Album(album);
    }

    fn load_tracks(&mut self, context: &mut Context, album: String, start: usize) {
        let Some(account) = &self.account else {
            return;
        };
        let url = api::tracks_url(SERVER, account, &album, start);
        self.fetch(context, url, Work::Tracks { album, start });
    }

    fn play(&mut self, context: &mut Context, index: usize) {
        self.play_from(context, index, None);
    }

    /// Plays the track at `index`, from `at` when carrying on.
    fn play_from(&mut self, context: &mut Context, index: usize, at: Option<u64>) {
        let (Some(entry), Some(account), View::Album(album)) =
            (self.tracks.entries.get(index), &self.account, &self.view)
        else {
            return;
        };
        let url = api::stream_url(SERVER, account, &entry.id);
        let by = if entry.by.is_empty() {
            &album.by
        } else {
            &entry.by
        };
        let mut metadata = AudioMetadata::new(&entry.name);
        if !by.is_empty() {
            metadata = metadata.author(by);
        }
        let mut player = AudioPlayer::stream(url, &entry.name)
            .metadata(metadata)
            .owns_back(true);
        if let Some(ms) = at {
            player = player.resuming_at(std::time::Duration::from_millis(ms));
        }
        player.set_cover(self.cover_of(&album.id));
        let track = entry.id.clone();
        let album = album.clone();
        self.stop_playing(context);
        player.start(context);
        player.play(context);
        context.log(
            LogLevel::Info,
            format!("pharos: play track={track} from_ms={}", at.unwrap_or(0)),
        );
        self.playing = Some(Playing {
            player,
            album,
            track,
            sound_reported: false,
        });
        self.view = View::Player;
        self.report(context, Report::Started);
        self.report_timer = self.sleep(context, REPORT_SECONDS, Work::ReportTimer);
    }

    fn stop_playing(&mut self, context: &mut Context) {
        if self.playing.is_some() {
            self.report(context, Report::Stopped);
        }
        if let Some(playing) = self.playing.as_mut() {
            playing.player.pause(context);
        }
        self.playing = None;
        if let Some(timer) = self.report_timer.take() {
            context.cancel(timer);
        }
    }

    /// Tells pharos where the listener is. A report that fails is logged and
    /// not retried: the next one carries a newer place anyway.
    fn report(&mut self, context: &mut Context, report: Report) {
        let Some(playing) = self.playing.as_mut() else {
            return;
        };
        let position_ms = u64::try_from(playing.player.position().as_millis()).unwrap_or(u64::MAX);
        let paused = !playing.player.is_playing();
        playing.sound_reported = !paused;
        let body = api::report_body(&playing.track, position_ms, paused);
        self.post(context, report.url(SERVER), body, Work::Report(report));
    }

    fn completed(&mut self, context: &mut Context, work: Work, bytes: &[u8]) {
        match work {
            Work::Initiate => match api::parse_initiate(bytes) {
                Some((code, secret)) => {
                    self.view = View::SignIn { code: Some(code) };
                    self.sleep(context, POLL_SECONDS, Work::Wait { secret });
                }
                None => self.problem("Pharos sent back no sign-in code."),
            },
            Work::Wait { secret } => {
                if matches!(self.view, View::SignIn { .. }) {
                    let url = api::connect_url(SERVER, &secret);
                    self.fetch(context, url, Work::Poll { secret });
                }
            }
            Work::Poll { secret } => {
                if api::parse_connected(bytes) {
                    let body = api::finish_body(&secret);
                    self.post(context, api::finish_url(SERVER), body, Work::Finish);
                } else {
                    self.sleep(context, POLL_SECONDS, Work::Wait { secret });
                }
            }
            Work::Finish => match api::parse_account(bytes) {
                Some(account) => {
                    context.log(LogLevel::Info, "pharos: signed in");
                    context.store().save(ACCOUNT_KEY, account.encode());
                    self.account = Some(account);
                    self.load_views(context);
                }
                None => self.problem("Pharos approved the code but sent no sign-in."),
            },
            Work::Views => match api::parse_views(bytes) {
                Some(sections) => self.sections = Some(sections),
                None => self.problem("Pharos sent a list of libraries this app could not read."),
            },
            Work::Listening => self.listening_arrived(context, bytes),
            Work::ListeningAlbum { id } => {
                self.album_described(context, &id, api::parse_item(bytes));
            }
            Work::Cover { id } => self.cover_arrived(context, &id, bytes),
            Work::Shelf { kind, start } => self.shelf_arrived(context, kind, start, bytes),
            Work::Tracks { album, start } => {
                // A reply for an album the reader has since left is stale.
                if self.album.as_ref().is_none_or(|open| open.id != album)
                    || start != self.tracks.entries.len()
                {
                    return;
                }
                match api::parse_page(bytes) {
                    Some(page) => {
                        let got = page.entries.len();
                        self.tracks.total = page.total;
                        self.tracks.entries.extend(page.entries);
                        // An album is small: fetch the rest straight away.
                        let loaded = self.tracks.entries.len();
                        let more = got > 0 && loaded < self.tracks.total;
                        if more {
                            self.load_tracks(context, album, loaded);
                        }
                        self.carry_on(context, more);
                    }
                    None => self.problem("Pharos sent a list this app could not read."),
                }
            }
            Work::Report(report) => {
                context.log(
                    LogLevel::Info,
                    format!("pharos: report {} sent", report.label()),
                );
            }
            Work::DrawWait => {}
            Work::ReportTimer => {
                if self
                    .playing
                    .as_ref()
                    .is_some_and(|playing| playing.player.is_playing())
                {
                    self.report(context, Report::Progress);
                }
                if self.playing.is_some() {
                    self.report_timer = self.sleep(context, REPORT_SECONDS, Work::ReportTimer);
                }
            }
        }
    }

    /// Plays the track the album was opened to carry on from, once it has
    /// loaded and the listener has asked to; forgets it when the album turns
    /// out not to hold it.
    fn carry_on(&mut self, context: &mut Context, more: bool) {
        let Some((track, at)) = self.resume_at.clone() else {
            return;
        };
        match self
            .tracks
            .entries
            .iter()
            .position(|entry| entry.id == track)
        {
            Some(index) if self.resume_now => {
                self.resume_at = None;
                self.resume_now = false;
                self.play_from(context, index, Some(at));
            }
            None if !more => {
                context.log(
                    LogLevel::Info,
                    format!("pharos: resume track={track} not in its album"),
                );
                self.resume_at = None;
            }
            Some(_) | None => {}
        }
    }

    /// Takes one block of a shelf and asks for the next until the listing is
    /// read.
    fn shelf_arrived(&mut self, context: &mut Context, kind: Kind, start: usize, bytes: &[u8]) {
        // A reply for a shelf the reader has since left is stale.
        if !matches!(self.view, View::Library(open) if open == kind)
            || self.kind != kind
            || start != self.shelf_sent
        {
            return;
        }
        match api::parse_page(bytes) {
            Some(page) => {
                self.shelf_sent += page.sent;
                self.shelf.total = page.total;
                self.shelf
                    .entries
                    .extend(page.entries.into_iter().filter(|entry| kind.keeps(entry)));
                self.shelf_done =
                    page.sent == 0 || self.shelf_sent >= page.total || self.shelf_sent >= MAX_SHELF;
                self.more_shelf(context);
            }
            None => self.problem("Pharos sent a list this app could not read."),
        }
    }

    fn failed(&mut self, context: &mut Context, work: Work, error: TaskError) {
        context.log(
            LogLevel::Info,
            format!("pharos: {} failed: {error:?}", work_label(&work)),
        );
        match (work, error) {
            // A cover that fails keeps its glyph and nothing else changes.
            (Work::Cover { id }, error) => self.cover_failed(context, &id, &format!("{error:?}")),
            (_, TaskError::Unauthorized) if self.account.is_some() => {
                // Signed out on the server (the device removed, or the
                // password changed): forget the token and sign in again.
                self.sign_out(context);
            }
            // A lost report only costs the server a moment of the listener's
            // place, and the home screen does without its Continue listening
            // row until it is shown again.
            (Work::Report(_) | Work::ReportTimer | Work::DrawWait | Work::Listening, _) => {}
            // The album keeps the name its track gave it, and the glyph.
            (Work::ListeningAlbum { id }, _) => self.album_described(context, &id, None),
            // The shelf keeps what loaded, and the rest is read when it is
            // opened again.
            (Work::Shelf { start, .. }, _) if start > 0 => self.shelf_done = true,
            // A later block of tracks fails in the background, maybe while the
            // listener is in the player: keep what loaded, and opening the
            // album again fetches the rest.
            (Work::Tracks { start, .. }, _) if start > 0 => {}
            // A code nobody approved expires; ask for a fresh one.
            (Work::Poll { .. }, TaskError::NotFound) => self.sign_in(context),
            (_, TaskError::Offline) => {
                self.problem("The reader is offline. Turn on Wi-Fi and try again.");
            }
            (_, error) => self.problem(format!("Pharos could not be reached ({error:?}).")),
        }
    }

    fn problem(&mut self, why: impl Into<String>) {
        self.view = View::Problem(why.into());
    }

    /// The second line of a track row: who, and how long.
    fn summary(entry: &Entry) -> String {
        let mut summary = entry.by.clone();
        if let Some(ms) = entry.runtime_ms {
            if !summary.is_empty() {
                summary.push_str(" · ");
            }
            summary.push_str(&api::clock(ms));
        }
        summary
    }

    /// The track rows at `indices`, numbered from one.
    fn rows(page: &Page, indices: &[usize]) -> Vec<(String, String, String, u16)> {
        indices
            .iter()
            .filter_map(|&index| {
                let entry = page.entries.get(index)?;
                Some((
                    format!("row-{index}"),
                    entry.name.clone(),
                    Self::summary(entry),
                    u16::try_from(index + 1).unwrap_or(u16::MAX),
                ))
            })
            .collect()
    }

    /// The top of an album's first page: the cover beside the title, the
    /// artist, the year and the length.
    fn masthead(&self, album: &Entry) -> ScreenBuilder {
        let mut facts = Vec::new();
        if let Some(year) = album.year {
            facts.push(("Year", year.to_string()));
        }
        if let Some(ms) = album.runtime_ms {
            facts.push(("Length", api::clock(ms)));
        }
        let by = (!album.by.is_empty()).then(|| album.by.clone());
        let teaser = about::teaser(&album.overview, about::TEASER_CHARS);
        let mut screen =
            self.now_playing(ScreenBuilder::new("pharos-album").top_bar(self.kind.label()));
        if matches!(teaser, Some((_, true))) {
            screen = screen.top_bar_action("about", "About");
        }
        let screen = screen.hero(
            self.cover_of(&album.id),
            HERO_COVER_MM,
            album.name.clone(),
            by,
            facts,
        );
        let screen = match teaser {
            Some((shown, _)) => screen.text(shown),
            None => screen,
        };
        // Opened from Continue listening: the way to carry on, on the page,
        // as well as the hold on the home tile.
        match &self.resume_at {
            Some((track, at)) => {
                let number = self
                    .tracks
                    .entries
                    .iter()
                    .position(|entry| entry.id == *track)
                    .map(|index| format!("track {} ", index + 1))
                    .unwrap_or_default();
                screen.primary_button("resume", format!("Resume {number}at {}", api::clock(*at)))
            }
            None => screen,
        }
    }

    /// The tracks split into pages. The first page sits under the masthead,
    /// so it holds fewer rows than the rest; its room is measured, not guessed.
    fn track_pages(&self, context: &Context, album: &Entry) -> Vec<Vec<usize>> {
        let summaries: Vec<String> = self.tracks.entries.iter().map(Self::summary).collect();
        let sectioned: Vec<(Option<&str>, &str, &str)> = self
            .tracks
            .entries
            .iter()
            .zip(&summaries)
            .map(|(entry, summary)| (None, entry.name.as_str(), summary.as_str()))
            .collect();
        let placed = self.masthead(album).build();
        let under = context.paginate_rows_in_sections_under(
            &sectioned,
            false,
            Position::AtTheFoot,
            &placed,
        );
        let first = under.into_iter().next().unwrap_or_default();
        let taken = first.len();
        let rest: Vec<(&str, &str)> = sectioned
            .iter()
            .skip(taken)
            .map(|&(_, name, summary)| (name, summary))
            .collect();
        let mut pages = vec![first];
        for page in context.paginate_rows(&rest, false) {
            if !page.is_empty() {
                pages.push(page.into_iter().map(|index| index + taken).collect());
            }
        }
        pages
    }

    fn screen(&self, context: &Context) -> Screen {
        match &self.view {
            View::Opening => ScreenBuilder::new("pharos").top_bar("Pharos").build(),
            View::SignIn { code: None } => ScreenBuilder::new("pharos-sign-in")
                .top_bar("Sign in to Pharos")
                .activity("Asking Pharos for a sign-in code", None)
                .build(),
            View::SignIn { code: Some(code) } => ScreenBuilder::new("pharos-sign-in")
                .top_bar("Sign in to Pharos")
                .heading(code.clone())
                .text(
                    "On a phone or computer signed in to Pharos, open Quick Connect \
                     and enter this code.",
                )
                .build(),
            View::Home => self.home_screen(context),
            View::Library(kind) if self.shelf.entries.is_empty() => {
                let screen =
                    self.now_playing(ScreenBuilder::new("pharos-library").top_bar(kind.label()));
                let screen = if self.shelf_done {
                    screen.text(format!("No {} here yet.", kind.noun()))
                } else {
                    screen.activity(format!("Loading your {}", kind.noun()), None)
                };
                screen.build().with_own_back(true)
            }
            View::Library(kind) => self
                .now_playing(ScreenBuilder::new("pharos-library").top_bar(format!(
                    "{} · {} {}",
                    kind.label(),
                    self.shelf_counted,
                    kind.noun()
                )))
                .top_bar_action("letters", "A–Z")
                .picture_tiles(Self::shape(*kind), self.tiles(context))
                .page_turns("page-back", "page-next")
                .compose(|screen| self.letter_picker(context, screen))
                .build()
                .with_own_back(true),
            View::Books => self.reading.screen(context, &|id| self.cover_of(id)),
            View::Album(album) if self.about.is_some() => self
                .now_playing(about::screen(
                    context,
                    "pharos-album-about",
                    &album.name,
                    &album.overview,
                    self.about.unwrap_or_default(),
                    ("page-back", "page-next"),
                ))
                .build()
                .with_own_back(true),
            View::Album(album) if self.tracks.entries.is_empty() => self
                .masthead(album)
                .activity("Loading the tracks", None)
                .build()
                .with_own_back(true),
            View::Album(album) => {
                let pages = self.track_pages(context, album);
                let page = self.page.min(pages.len().saturating_sub(1));
                let indices = pages.get(page).map_or(&[][..], Vec::as_slice);
                // Only the first page has room for the masthead.
                let screen = if page == 0 {
                    self.masthead(album)
                } else {
                    self.now_playing(ScreenBuilder::new("pharos-album").top_bar(album.name.clone()))
                };
                screen
                    .rows(Self::rows(&self.tracks, indices))
                    .page_turns("page-back", "page-next")
                    .build()
                    .with_own_back(true)
            }
            View::Player => match &self.playing {
                Some(playing) => playing.player.screen_for(context),
                None => ScreenBuilder::new("pharos")
                    .error_state("Nothing is playing.")
                    .build(),
            },
            View::Problem(why) => ScreenBuilder::new("pharos-problem")
                .top_bar("Pharos")
                .error_state(why.clone())
                .primary_button("retry", "Try again")
                .build(),
        }
    }

    /// The tiles of the shelf page showing, each with its cover when it has
    /// arrived and its glyph until then.
    fn tiles(&self, context: &Context) -> Vec<(String, String, Glyph, Option<TilePicture>)> {
        let pages = self.grid_pages(context);
        let page = self.shelf_page.min(pages.len().saturating_sub(1));
        pages
            .get(page)
            .into_iter()
            .flatten()
            .filter_map(|&index| {
                let entry = self.shelf.entries.get(index)?;
                Some((
                    format!("tile-{index}"),
                    entry.name.clone(),
                    Self::glyph(self.kind),
                    self.cover_of(&entry.id),
                ))
            })
            .collect()
    }

    /// Where each letter starts on the shelf (V1442).
    fn letter_starts(&self) -> Vec<(char, usize)> {
        letters::starts(self.shelf.entries.iter().map(|entry| entry.name.as_str()))
    }

    /// The letters the shelf holds, over the shelf while the reader picks
    /// one. The letter of the page showing is marked.
    fn letter_picker(&self, context: &Context, screen: ScreenBuilder) -> ScreenBuilder {
        if !self.picking_letter {
            return screen;
        }
        let pages = self.grid_pages(context);
        let page = self.shelf_page.min(pages.len().saturating_sub(1));
        let showing = pages
            .get(page)
            .and_then(|page| page.first())
            .and_then(|&index| self.shelf.entries.get(index))
            .map(|entry| letters::letter_of(&entry.name));
        letters::picker(screen, &self.letter_starts(), showing)
    }

    /// Answers the letter picker: a letter turns the shelf to the page its
    /// first album is on, and Back closes it. Whether the action was its.
    fn pick_letter(&mut self, context: &Context, action: ActionId) -> bool {
        if !matches!(self.view, View::Library(_)) {
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
            if let Some(page) = letters::page_holding(&self.grid_pages(context), index) {
                self.shelf_page = page;
            }
            return true;
        }
        // Anything else on the screen closes the picker and does its own job.
        false
    }

    fn glyph(kind: Kind) -> Glyph {
        match kind {
            Kind::Music => Glyph::Music,
            Kind::Audiobooks => Glyph::Headphones,
            Kind::Books => Glyph::Books,
        }
    }

    /// The tiles of the home screen's Continue listening row.
    #[allow(clippy::type_complexity)]
    fn listening_tiles(
        &self,
        count: usize,
    ) -> Vec<(
        String,
        String,
        String,
        Glyph,
        String,
        Option<TilePicture>,
        TileState,
    )> {
        self.listening
            .iter()
            .take(count)
            .enumerate()
            .map(|(index, resumed)| {
                let glyph = if resumed.album.audiobook {
                    Glyph::Headphones
                } else {
                    Glyph::Music
                };
                (
                    format!("listen-{index}"),
                    resumed.album.name.clone(),
                    resumed.album.by.clone(),
                    glyph,
                    format!("listen-hold-{index}"),
                    self.cover_of(&format!("{HOME_COVER}{}", resumed.album.id)),
                    TileState::Normal,
                )
            })
            .collect()
    }

    /// The tiles of the Continue reading row, with each book's id.
    #[allow(clippy::type_complexity)]
    fn reading_tiles(
        &self,
        count: usize,
    ) -> Vec<(
        String,
        String,
        String,
        Glyph,
        String,
        Option<TilePicture>,
        TileState,
    )> {
        self.reading
            .recent()
            .into_iter()
            .take(count)
            .enumerate()
            .map(|(index, book)| {
                (
                    format!("read-{index}"),
                    book.title.clone(),
                    book.author.clone(),
                    Glyph::Book,
                    format!("read-hold-{index}"),
                    self.cover_of(&format!("{HOME_COVER}{}", book.id)),
                    TileState::Normal,
                )
            })
            .collect()
    }

    /// How many tiles each recent row shows: one row's worth, and a row left
    /// out, reading first, when the screen would not fit the panel with it.
    /// The layout engine is the judge, as it is of every other screen.
    fn home_rows(&self, context: &Context) -> (usize, usize) {
        let columns = Self::home_columns(context);
        let listening = self.listening.len().min(columns);
        let reading = self.reading.recent().len().min(columns);
        let metrics = context.metrics();
        [(listening, reading), (listening, 0), (0, reading)]
            .into_iter()
            .find(|&(listening, reading)| {
                self.home_with(listening, reading)
                    .validate(&metrics)
                    .is_empty()
            })
            .unwrap_or((0, 0))
    }

    /// The top of the home screen: its bar and the recent rows that have
    /// something in them.
    fn recent_rows(&self, listening: usize, reading: usize) -> ScreenBuilder {
        let mut screen = self.now_playing(ScreenBuilder::new("pharos-home").top_bar("Pharos"));
        if listening > 0 {
            screen = screen
                .section("Continue listening")
                .contextual_picture_tiles(TileShape::Square, self.listening_tiles(listening));
        }
        if reading > 0 {
            screen = screen
                .section("Continue reading")
                .contextual_picture_tiles(TileShape::Square, self.reading_tiles(reading));
        }
        if listening + reading > 0 {
            screen = screen.section("Libraries");
        }
        screen
    }

    /// The covers on the home screen, as `(id, cover tag)`.
    fn home_covers(&self, context: &Context) -> Vec<(String, Option<String>)> {
        let (listening, reading) = self.home_rows(context);
        self.listening
            .iter()
            .take(listening)
            .map(|resumed| (resumed.album.id.clone(), resumed.album.image_tag.clone()))
            .chain(
                self.reading
                    .recent()
                    .into_iter()
                    .take(reading)
                    .map(|book| (book.id, book.image_tag)),
            )
            .map(|(id, tag)| (format!("{HOME_COVER}{id}"), tag))
            .collect()
    }

    /// What was last listened to and read, then one tile for each kind of
    /// library the owner can see (V1445).
    fn home_screen(&self, context: &Context) -> Screen {
        let (listening, reading) = self.home_rows(context);
        self.home_with(listening, reading)
    }

    /// The home screen with this many tiles in each recent row.
    fn home_with(&self, listening: usize, reading: usize) -> Screen {
        let screen = self.recent_rows(listening, reading);
        let screen = match &self.sections {
            None => screen.activity("Loading your libraries", None),
            Some(sections) if sections.is_empty() => {
                screen.text("Pharos shows no music, audiobooks or books for this account.")
            }
            Some(sections) => screen.tiles(sections.iter().map(|section| {
                (
                    format!("section-{}", section.kind.label().to_lowercase()),
                    section.kind.label(),
                    Self::glyph(section.kind),
                )
            })),
        };
        screen.bottom_action("sign-out", "Sign out").build()
    }

    /// Answers a tap on the home screen: a recent tile, or a section.
    fn home_action(&mut self, context: &mut Context, action: ActionId) {
        if self.open_recent(context, action) {
            return;
        }
        let kinds = [Kind::Music, Kind::Audiobooks, Kind::Books];
        if let Some(kind) = kinds
            .into_iter()
            .find(|kind| action == action_id(&format!("section-{}", kind.label().to_lowercase())))
        {
            self.open_section(context, kind);
        }
    }

    /// Answers a recent tile, as every Pharos home screen does: a tap opens
    /// the album's or the book's page, which offers Resume or Read, and a
    /// hold carries on at once. Whether the action was one.
    fn open_recent(&mut self, context: &mut Context, action: ActionId) -> bool {
        let (listening, reading) = self.home_rows(context);
        let tapped = Self::indexed(action, "listen", listening).map(|index| (index, false));
        let held = Self::indexed(action, "listen-hold", listening).map(|index| (index, true));
        if let Some((index, now)) = tapped.or(held) {
            if let Some(resumed) = self.listening.get(index).cloned() {
                self.kind = if resumed.album.audiobook {
                    Kind::Audiobooks
                } else {
                    Kind::Music
                };
                context.log(
                    LogLevel::Info,
                    format!(
                        "pharos: home open album={} track={} at_ms={} play_now={now}",
                        resumed.album.id, resumed.track, resumed.position_ms
                    ),
                );
                self.open_album(context, resumed.album);
                self.resume_at = Some((resumed.track, resumed.position_ms));
                self.resume_now = now;
                self.album_from_home = true;
            }
            return true;
        }
        let tapped = Self::indexed(action, "read", reading).map(|index| (index, false));
        let held = Self::indexed(action, "read-hold", reading).map(|index| (index, true));
        if let Some((index, now)) = tapped.or(held) {
            let book = self.reading.recent().into_iter().nth(index);
            if let (Some(book), Some(account)) = (book, self.account.clone()) {
                context.log(
                    LogLevel::Info,
                    format!("pharos: home open book={} read_now={now}", book.id),
                );
                let opened = if now {
                    self.reading.continue_book(context, &account, &book.id)
                } else {
                    self.reading.show_book(context, &account, &book.id)
                };
                if opened {
                    self.view = View::Books;
                }
            }
            return true;
        }
        false
    }

    /// Adds the way back to the player while something plays: Back from the
    /// player keeps the track going, and choosing it again would restart it.
    fn now_playing(&self, screen: ScreenBuilder) -> ScreenBuilder {
        if self.playing.is_some() {
            screen.top_bar_action("now-playing", "Playing")
        } else {
            screen
        }
    }

    fn show(&mut self, context: &mut Context) {
        self.end_draw_wait(context);
        self.more_shelf(context);
        self.shelf_counted = self.shelf.entries.len();
        let screen = self.screen(context);
        self.shown = Some(screen.clone());
        context.set_screen(screen);
    }

    /// [`Self::show`], unless the screen is the one already showing. The
    /// runtime re-renders and re-compares the whole panel for every screen it
    /// is sent, so a cover arriving for a page not on show, or a report
    /// answered, would cost a full redraw for nothing. Only for task answers:
    /// a screen after input is always sent, because the runtime waits on one
    /// after handing Back over.
    /// Whether a screen was sent.
    fn show_if_changed(&mut self, context: &mut Context) -> bool {
        self.end_draw_wait(context);
        self.more_shelf(context);
        let screen = self.screen(context);
        if self.shown.as_ref() == Some(&screen) {
            return false;
        }
        let counted = self.shelf.entries.len();
        let screen = if self.shelf_counted == counted {
            screen
        } else {
            self.shelf_counted = counted;
            self.screen(context)
        };
        self.shown = Some(screen.clone());
        context.set_screen(screen);
        true
    }

    /// Starts the wait for what the screen shows, unless one is running.
    /// Without a task slot to spare it draws now rather than wait unbounded.
    fn begin_draw_wait(&mut self, context: &mut Context) -> bool {
        if self.draw_wait.is_some() {
            return true;
        }
        self.draw_wait = self.sleep(context, DRAW_WAIT_SECONDS, Work::DrawWait);
        self.draw_wait.is_some()
    }

    fn end_draw_wait(&mut self, context: &mut Context) {
        if let Some(task) = self.draw_wait.take() {
            context.cancel(task);
            self.tasks.retain(|(held, _)| *held != task);
        }
    }

    /// Draws what input asked for. A page opened or turned to waits for its
    /// albums and covers and is drawn once with them, not once bare and again
    /// as they land (B1458). Back is always answered at once: the runtime
    /// waits on a screen for it.
    fn answer_input(&mut self, context: &mut Context, action: ActionId) {
        if action != ActionId::BACK && self.screen_loading(context) {
            if self.begin_draw_wait(context) {
                self.log_draw(context, "held", "input");
                return;
            }
            // No task slot for the wait: drawing now beats waiting unbounded.
            self.log_draw(context, "drawn-no-slot", "input");
        } else {
            self.log_draw(context, "drawn", "input");
        }
        self.show(context);
    }

    /// Records a draw decision for the frame log: what was decided, what
    /// prompted it, and what the screen still waited on (B1458).
    fn log_draw(&self, context: &mut Context, verdict: &str, cause: &str) {
        let view = match &self.view {
            View::Opening => "opening",
            View::SignIn { .. } => "sign-in",
            View::Home => "home",
            View::Library(_) => "shelf",
            View::Books => "books",
            View::Album(_) => "album",
            View::Player => "player",
            View::Problem(_) => "problem",
        };
        let message = format!(
            "pharos: draw {verdict} view={view} cause={cause} page={} in_flight={} covers_pending={} waiting={}",
            self.shelf_page,
            context.tasks_in_flight(),
            self.page_covers_pending(context),
            self.draw_wait.is_some(),
        );
        context.log(LogLevel::Info, message);
    }

    /// Which of `visible` things an action named `{prefix}-{index}` chose.
    fn indexed(action: ActionId, prefix: &str, visible: usize) -> Option<usize> {
        (0..visible).find(|index| action == action_id(&format!("{prefix}-{index}")))
    }

    fn turn(&mut self, context: &Context, forward: bool) {
        let pages = match &self.view {
            View::Library(_) => self.grid_pages(context).len().max(1),
            View::Album(album) if self.about.is_some() => {
                about::page_count(context, &album.overview)
            }
            View::Album(album) => self.track_pages(context, album).len().max(1),
            _ => return,
        };
        let current = match (&self.view, self.about.as_mut()) {
            (View::Library(_), _) => &mut self.shelf_page,
            (_, Some(about)) => about,
            _ => &mut self.page,
        };
        if forward && *current + 1 < pages {
            *current += 1;
        } else if !forward && *current > 0 {
            *current -= 1;
        }
    }
}

fn work_label(work: &Work) -> &'static str {
    match work {
        Work::Initiate => "initiate",
        Work::Wait { .. } => "wait",
        Work::Poll { .. } => "poll",
        Work::Finish => "finish",
        Work::Views => "views",
        Work::Listening => "listening",
        Work::ListeningAlbum { .. } => "listening-album",
        Work::Shelf { .. } => "shelf",
        Work::Cover { .. } => "cover",
        Work::Tracks { .. } => "tracks",
        Work::Report(report) => report.label(),
        Work::ReportTimer => "report-timer",
        Work::DrawWait => "draw-wait",
    }
}

impl KoboApp for Pharos {
    fn on_start(&mut self, context: &mut Context) {
        context.store().load(ACCOUNT_KEY);
        self.show(context);
    }

    fn on_store(&mut self, context: &mut Context, result: StoreResult) {
        if let StoreResult::Loaded { key, value } = result {
            if key != ACCOUNT_KEY {
                return;
            }
            match value.as_deref().and_then(Account::decode) {
                Some(account) => {
                    self.account = Some(account);
                    self.load_views(context);
                    // An empty home drawn now would cost a panel update of
                    // its own before the filled-in one.
                    if self.home_loading(context) && self.begin_draw_wait(context) {
                        return;
                    }
                }
                None => self.sign_in(context),
            }
            self.show(context);
        }
    }

    fn on_load(&mut self, context: &mut Context, key: &str, result: StoreResult) {
        if key == books::KEY {
            self.reading.loaded(&result);
            self.reading.fetch_known(context);
            self.refresh_covers(context);
            if matches!(self.view, View::Home) {
                if !self.home_loading(context) {
                    self.show_if_changed(context);
                }
                return;
            }
            self.show(context);
        } else {
            self.on_store(context, result);
        }
    }

    fn on_save(&mut self, context: &mut Context, key: &str, result: StoreResult) {
        if key == books::KEY {
            self.reading.saved(context, &result);
            self.show(context);
        } else {
            self.on_store(context, result);
        }
    }

    fn on_shelf(&mut self, context: &mut Context, _name: &str, result: StoreResult) {
        if self.reading.shelf(context, &result) {
            self.show(context);
        }
    }

    fn on_action(&mut self, context: &mut Context, action: ActionId) {
        // Input is always answered with a draw, and the wait's task slot is
        // wanted for what the input asks for.
        self.end_draw_wait(context);
        if matches!(self.view, View::Books) {
            match self.reading.action(context, action) {
                Leave::Back => self.go_home(context),
                Leave::SignOut => self.sign_out(context),
                Leave::Stay => {}
            }
            self.refresh_covers(context);
            self.show(context);
            return;
        }
        if matches!(self.view, View::Player) {
            if action == ActionId::BACK {
                // Backing out keeps the track playing, as the player says.
                self.view = self
                    .album
                    .clone()
                    .map_or(View::Library(self.kind), View::Album);
                self.show(context);
                return;
            }
            if let Some(playing) = self.playing.as_mut() {
                let was_playing = playing.player.is_playing();
                if playing.player.press(context, action) {
                    if playing.player.is_playing() != was_playing {
                        self.report(context, Report::Progress);
                    }
                    self.show(context);
                    return;
                }
            }
        }
        if self.pick_letter(context, action) {
            self.refresh_covers(context);
            self.show(context);
            return;
        }
        if action == ActionId::BACK {
            match self.view {
                View::Album(_) if self.about.is_some() => self.about = None,
                View::Album(_) => {
                    self.album = None;
                    self.page = 0;
                    self.resume_at = None;
                    if std::mem::take(&mut self.album_from_home) {
                        self.go_home(context);
                    } else {
                        self.view = View::Library(self.kind);
                    }
                }
                View::Library(_) => self.go_home(context),
                _ => {}
            }
        } else if action == action_id("retry") {
            match self.account {
                Some(_) => self.load_views(context),
                None => self.sign_in(context),
            }
        } else if action == action_id("resume") && matches!(self.view, View::Album(_)) {
            self.resume_now = true;
            let more = self.tracks.total == 0 || self.tracks.entries.len() < self.tracks.total;
            self.carry_on(context, more);
        } else if action == action_id("about") && matches!(self.view, View::Album(_)) {
            self.about = Some(0);
        } else if action == action_id("now-playing") && self.playing.is_some() {
            self.view = View::Player;
        } else if action == action_id("sign-out") {
            self.sign_out(context);
        } else if action == action_id("page-next") {
            self.turn(context, true);
        } else if action == action_id("page-back") {
            self.turn(context, false);
        } else {
            let visible = match &self.view {
                View::Library(_) => self.shelf.entries.len(),
                View::Album(_) => self.tracks.entries.len(),
                _ => 0,
            };
            if matches!(self.view, View::Home) {
                self.home_action(context, action);
            }
            match &self.view {
                View::Library(_) => {
                    if let Some(index) = Self::indexed(action, "tile", visible) {
                        if let Some(album) = self.shelf.entries.get(index).cloned() {
                            self.open_album(context, album);
                        }
                    }
                }
                View::Album(_) => {
                    if let Some(index) = Self::indexed(action, "row", visible) {
                        self.play(context, index);
                    }
                }
                _ => {}
            }
        }
        self.refresh_covers(context);
        self.answer_input(context, action);
    }

    fn on_device_result(
        &mut self,
        context: &mut Context,
        request: DeviceRequest,
        result: DeviceResult,
    ) {
        if let Some(playing) = self.playing.as_mut() {
            if playing.player.on_device_result(context, &request, &result) {
                // The first moment sound really runs is worth a report: the
                // started one went before the stream had loaded.
                if playing.player.is_playing() && !playing.sound_reported {
                    self.report(context, Report::Progress);
                }
                if matches!(self.view, View::Player) {
                    self.show(context);
                }
            }
        }
    }

    fn on_task(&mut self, context: &mut Context, task: TaskId, outcome: TaskOutcome) {
        if let Some(playing) = self.playing.as_mut() {
            if playing.player.on_task(context, task, &outcome) {
                if matches!(self.view, View::Player) {
                    self.show(context);
                }
                return;
            }
        }
        if let Some(leave) = self.reading.task(context, task, &outcome) {
            if leave == Leave::SignOut {
                self.sign_out(context);
            }
            self.refresh_covers(context);
            if matches!(self.view, View::Home) {
                if !self.home_loading(context) {
                    self.show_if_changed(context);
                }
                return;
            }
            self.show(context);
            return;
        }
        let Some(position) = self.tasks.iter().position(|(id, _)| *id == task) else {
            return;
        };
        let (_, work) = self.tasks.swap_remove(position);
        let cause = work_label(&work);
        let was_cover = matches!(work, Work::Cover { .. });
        let waited = matches!(work, Work::DrawWait);
        if waited {
            self.draw_wait = None;
        }
        if self.report_timer == Some(task) {
            self.report_timer = None;
        }
        match outcome {
            TaskOutcome::Completed(bytes) => self.completed(context, work, &bytes),
            TaskOutcome::Failed(error) => self.failed(context, work, error),
            TaskOutcome::Cancelled => {}
        }
        self.refresh_covers(context);
        // The runtime waits out each panel update before it reads the next
        // message, so a page whose covers were drawn as each arrived took one
        // panel update per cover. They are drawn together once all are in.
        if was_cover && self.page_covers_pending(context) {
            self.log_draw(context, "held", cause);
            return;
        }
        // The home screen is drawn once, when what it lists and the covers
        // it shows are all in, not once per answer on the way (B1457).
        if !waited && matches!(self.view, View::Home) && self.home_loading(context) {
            self.log_draw(context, "held", cause);
            return;
        }
        // A shelf page is drawn once its albums and covers are in.
        if !waited && matches!(self.view, View::Library(_)) && self.shelf_loading(context) {
            self.log_draw(context, "held", cause);
            return;
        }
        let verdict = if self.show_if_changed(context) {
            "drawn"
        } else {
            "unchanged"
        };
        self.log_draw(context, verdict, cause);
    }
}

fn main() -> ExitCode {
    match kobo_sdk::run("pharos", Pharos::default()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pharos: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;
