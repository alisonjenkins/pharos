//! Pharos on a Kobo (spec 047, ADR-0136).
//!
//! Signs in to the owner's pharos with a Quick Connect code, shows a home
//! screen with a section for each kind of library the owner can see, lists
//! the albums and audiobooks in one, and plays a track through the runtime's Bluetooth audio, telling
//! pharos where the listener is so any other device can carry on.
//!
//! The server address is fixed when the package is built
//! (`PHAROS_KOBO_SERVER`); the reader has one owner and one server.

mod api;
mod books;
mod reading;

use api::{Account, Cover, CoverCache, Entry, Kind, Page, Report, Section};
use kobo_sdk::audio::{AudioMetadata, AudioPlayer};
use kobo_sdk::prelude::*;
use kobo_sdk::{
    Glyph, Header, LogLevel, PictureHandle, Position, Task, TaskError, TaskId, TaskOutcome,
    TilePicture, TileShape, MAX_TASKS_IN_FLIGHT,
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

#[derive(Debug)]
enum View {
    /// Reading the saved account, before anything is known.
    Opening,
    /// Asking for, or showing, a Quick Connect code.
    SignIn {
        code: Option<String>,
    },
    /// The sections: one for each kind of library the owner can see.
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
}

struct Playing {
    player: AudioPlayer,
    /// The album the track belongs to, whose cover the player shows.
    album: Entry,
    track: String,
    sound_reported: bool,
}

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
    shelf_page: usize,
    covers: CoverCache,
    /// The screen last handed to the runtime, so a task that changes nothing
    /// on it does not make the runtime draw the whole panel again.
    shown: Option<Screen>,
    tracks: Page,
    page: usize,
    /// The album whose tracks are loaded, for Back from the player.
    album: Option<Entry>,
    playing: Option<Playing>,
    report_timer: Option<TaskId>,
    reading: Reading,
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
            shelf_page: 0,
            covers: CoverCache::new(MIN_COVERS),
            shown: None,
            tracks: Page::default(),
            page: 0,
            album: None,
            playing: None,
            report_timer: None,
            reading: Reading::new(SERVER, VERSION),
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
        self.view = View::Home;
        self.sections = None;
        self.fetch(context, url, Work::Views);
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

    /// The pixel size a cover is asked for and fitted to: the tile it fills.
    fn cover_size(&self, context: &Context) -> (u32, u32) {
        let metrics = context.metrics();
        let shape = Self::shape(self.kind);
        let margin = metrics.screen_margin();
        let columns = metrics.grid_columns_for_width(shape, metrics.width - 2 * margin);
        api::cover_px(
            metrics.width,
            margin,
            metrics.tenth_mm(20),
            columns,
            self.kind.portrait(),
        )
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
        let (mut wanted, per_page) = match &self.view {
            View::Album(album) => (tagged(album).into_iter().collect::<Vec<_>>(), 0),
            View::Library(_) => {
                let pages = self.grid_pages(context);
                let page = self.shelf_page.min(pages.len().saturating_sub(1));
                let wanted = api::wanted(&pages, page)
                    .into_iter()
                    .filter_map(|index| self.shelf.entries.get(index))
                    .filter_map(tagged)
                    .collect();
                (wanted, pages.first().map_or(0, Vec::len))
            }
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
        let (width, height) = self.cover_size(context);
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
            let url = api::image_url(SERVER, id, Some(tag), width, height);
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

    /// Whether a cover on the shelf page showing is still to come: asked
    /// for and not here yet, or not asked for yet. A cover that failed is
    /// not waited for.
    fn page_covers_pending(&self, context: &Context) -> bool {
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
        let (width, height) = self.cover_size(context);
        let colour = context.panel_shows_colour();
        let decoded = if colour {
            kobo_image::decode_colour(bytes)
        } else {
            kobo_image::decode(bytes)
        };
        let picture = match decoded.and_then(|picture| picture.cover(width, height)) {
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

    fn clear_shelf(&mut self) {
        self.shelf = Page::default();
        self.shelf_sent = 0;
        self.shelf_done = false;
        self.shelf_page = 0;
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
        player.set_cover(self.cover_of(&album.id));
        let track = entry.id.clone();
        let album = album.clone();
        self.stop_playing(context);
        player.start(context);
        player.play(context);
        context.log(LogLevel::Info, format!("pharos: play track={track}"));
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
                        if got > 0 && loaded < self.tracks.total {
                            self.load_tracks(context, album, loaded);
                        }
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
                if !self.shelf_done {
                    if let Some(library) = self.shelf_library() {
                        let next = self.shelf_sent;
                        self.load_shelf(context, &library, next);
                    }
                }
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
            // A lost report only costs the server a moment of the listener's place.
            (Work::Report(_) | Work::ReportTimer, _) => {}
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
        self.now_playing(ScreenBuilder::new("pharos-album").top_bar(self.kind.label()))
            .hero(
                self.cover_of(&album.id),
                HERO_COVER_MM,
                album.name.clone(),
                by,
                facts,
            )
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
            View::Home => self.home_screen(),
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
                    self.shelf.entries.len(),
                    kind.noun()
                )))
                .picture_tiles(Self::shape(*kind), self.tiles(context))
                .page_turns("page-back", "page-next")
                .build()
                .with_own_back(true),
            View::Books => self.reading.screen(),
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

    fn glyph(kind: Kind) -> Glyph {
        match kind {
            Kind::Music => Glyph::Music,
            Kind::Audiobooks => Glyph::Headphones,
            Kind::Books => Glyph::Books,
        }
    }

    /// One tile for each kind of library the owner can see.
    fn home_screen(&self) -> Screen {
        let screen = self.now_playing(ScreenBuilder::new("pharos-home").top_bar("Pharos"));
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
    fn show_if_changed(&mut self, context: &mut Context) {
        let screen = self.screen(context);
        if self.shown.as_ref() == Some(&screen) {
            return;
        }
        self.shown = Some(screen.clone());
        context.set_screen(screen);
    }

    /// Which of `visible` things an action named `{prefix}-{index}` chose.
    fn indexed(action: ActionId, prefix: &str, visible: usize) -> Option<usize> {
        (0..visible).find(|index| action == action_id(&format!("{prefix}-{index}")))
    }

    fn turn(&mut self, context: &Context, forward: bool) {
        let pages = match &self.view {
            View::Library(_) => self.grid_pages(context).len().max(1),
            View::Album(album) => self.track_pages(context, album).len().max(1),
            _ => return,
        };
        let current = if matches!(self.view, View::Library(_)) {
            &mut self.shelf_page
        } else {
            &mut self.page
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
        Work::Shelf { .. } => "shelf",
        Work::Cover { .. } => "cover",
        Work::Tracks { .. } => "tracks",
        Work::Report(report) => report.label(),
        Work::ReportTimer => "report-timer",
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
                }
                None => self.sign_in(context),
            }
            self.show(context);
        }
    }

    fn on_load(&mut self, context: &mut Context, key: &str, result: StoreResult) {
        if key == books::KEY {
            self.reading.loaded(&result);
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
        if matches!(self.view, View::Books) {
            match self.reading.action(context, action) {
                Leave::Back => self.view = View::Home,
                Leave::SignOut => self.sign_out(context),
                Leave::Stay => {}
            }
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
        if action == ActionId::BACK {
            match self.view {
                View::Album(_) => {
                    self.view = View::Library(self.kind);
                    self.album = None;
                    self.page = 0;
                }
                View::Library(_) => self.view = View::Home,
                _ => {}
            }
        } else if action == action_id("retry") {
            match self.account {
                Some(_) => self.load_views(context),
                None => self.sign_in(context),
            }
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
                let kinds = [Kind::Music, Kind::Audiobooks, Kind::Books];
                if let Some(kind) = kinds.into_iter().find(|kind| {
                    action == action_id(&format!("section-{}", kind.label().to_lowercase()))
                }) {
                    self.open_section(context, kind);
                }
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
        self.show(context);
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
            self.show(context);
            return;
        }
        let Some(position) = self.tasks.iter().position(|(id, _)| *id == task) else {
            return;
        };
        let (_, work) = self.tasks.swap_remove(position);
        let was_cover = matches!(work, Work::Cover { .. });
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
            return;
        }
        self.show_if_changed(context);
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
