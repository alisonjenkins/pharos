//! Pharos on a Kobo (spec 047, ADR-0136).
//!
//! Signs in to the owner's pharos with a Quick Connect code, lists the music
//! albums, and plays a track through the runtime's Bluetooth audio, telling
//! pharos where the listener is so any other device can carry on.
//!
//! The server address is fixed when the package is built
//! (`PHAROS_KOBO_SERVER`); the reader has one owner and one server.

mod api;

use api::{Account, Entry, Page, Report};
use kobo_sdk::audio::{AudioMetadata, AudioPlayer};
use kobo_sdk::prelude::*;
use kobo_sdk::{Header, LogLevel, Task, TaskError, TaskId, TaskOutcome};
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
/// Rows on one page of a list; the side of the panel turns to the next.
const ROWS_PER_PAGE: usize = 7;

#[derive(Debug)]
enum View {
    /// Reading the saved account, before anything is known.
    Opening,
    /// Asking for, or showing, a Quick Connect code.
    SignIn {
        code: Option<String>,
    },
    Albums,
    Album(Entry),
    Player,
    Problem(String),
}

/// What a network task was for.
#[derive(Clone, Debug)]
enum Work {
    Initiate,
    Wait { secret: String },
    Poll { secret: String },
    Finish,
    Albums { start: usize },
    Tracks,
    Report(Report),
    ReportTimer,
}

struct Playing {
    player: AudioPlayer,
    track: String,
    sound_reported: bool,
}

struct Pharos {
    view: View,
    account: Option<Account>,
    tasks: Vec<(TaskId, Work)>,
    albums: Page,
    tracks: Page,
    page: usize,
    /// The album whose tracks are loaded, for Back from the player.
    album: Option<Entry>,
    playing: Option<Playing>,
    report_timer: Option<TaskId>,
}

impl Default for Pharos {
    fn default() -> Self {
        Self {
            view: View::Opening,
            account: None,
            tasks: Vec::new(),
            albums: Page::default(),
            tracks: Page::default(),
            page: 0,
            album: None,
            playing: None,
            report_timer: None,
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
        self.albums = Page::default();
        context.store().forget(ACCOUNT_KEY);
        self.sign_in(context);
    }

    fn load_albums(&mut self, context: &mut Context, start: usize) {
        let Some(account) = &self.account else {
            return;
        };
        let url = api::albums_url(SERVER, account, start);
        self.fetch(context, url, Work::Albums { start });
    }

    fn open_album(&mut self, context: &mut Context, album: Entry) {
        let Some(account) = &self.account else {
            return;
        };
        let url = api::tracks_url(SERVER, account, &album.id);
        self.tracks = Page::default();
        self.page = 0;
        self.album = Some(album.clone());
        self.view = View::Album(album);
        self.fetch(context, url, Work::Tracks);
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
        let track = entry.id.clone();
        self.stop_playing(context);
        player.start(context);
        player.play(context);
        context.log(LogLevel::Info, format!("pharos: play track={track}"));
        self.playing = Some(Playing {
            player,
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
                    self.view = View::Albums;
                    self.load_albums(context, 0);
                }
                None => self.problem("Pharos approved the code but sent no sign-in."),
            },
            Work::Albums { start } => match api::parse_page(bytes) {
                Some(page) => {
                    if start == 0 {
                        self.albums = page;
                    } else {
                        self.albums.entries.extend(page.entries);
                        self.albums.total = page.total;
                    }
                }
                None => self.problem("Pharos sent a list this app could not read."),
            },
            Work::Tracks => match api::parse_page(bytes) {
                Some(page) => self.tracks = page,
                None => self.problem("Pharos sent a list this app could not read."),
            },
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

    fn failed(&mut self, context: &mut Context, work: Work, error: TaskError) {
        context.log(
            LogLevel::Info,
            format!("pharos: {} failed: {error:?}", work_label(&work)),
        );
        match (work, error) {
            (Work::Report(_) | Work::ReportTimer, _) => {}
            // A code nobody approved expires; ask for a fresh one.
            (Work::Poll { .. }, TaskError::NotFound) => self.sign_in(context),
            (_, TaskError::Unauthorized) if self.account.is_some() => {
                // Signed out on the server (the device removed, or the
                // password changed): forget the token and sign in again.
                self.sign_out(context);
            }
            (_, TaskError::Offline) => {
                self.problem("The reader is offline. Turn on Wi-Fi and try again.");
            }
            (_, error) => self.problem(format!("Pharos could not be reached ({error:?}).")),
        }
    }

    fn problem(&mut self, why: impl Into<String>) {
        self.view = View::Problem(why.into());
    }

    /// The rows of `page` on the panel page `self.page`.
    fn rows(page: &Page, at: usize, show_length: bool) -> Vec<(String, String, String, u16)> {
        page.entries
            .iter()
            .enumerate()
            .skip(at * ROWS_PER_PAGE)
            .take(ROWS_PER_PAGE)
            .map(|(index, entry)| {
                let mut summary = entry.by.clone();
                if let (true, Some(ms)) = (show_length, entry.runtime_ms) {
                    if !summary.is_empty() {
                        summary.push_str(" · ");
                    }
                    summary.push_str(&api::clock(ms));
                }
                (
                    format!("row-{index}"),
                    entry.name.clone(),
                    summary,
                    u16::try_from(index + 1).unwrap_or(u16::MAX),
                )
            })
            .collect()
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
            View::Albums if self.albums.entries.is_empty() => ScreenBuilder::new("pharos-albums")
                .top_bar("Music")
                .activity("Loading your albums", None)
                .button("sign-out", "Sign out")
                .build(),
            View::Albums => ScreenBuilder::new("pharos-albums")
                .top_bar(format!("Music · {} albums", self.albums.total))
                .rows(Self::rows(&self.albums, self.page, false))
                .page_turns("page-back", "page-next")
                .bottom_action("sign-out", "Sign out")
                .build(),
            View::Album(album) if self.tracks.entries.is_empty() => {
                ScreenBuilder::new("pharos-album")
                    .top_bar(album.name.clone())
                    .activity("Loading the tracks", None)
                    .build()
                    .with_own_back(true)
            }
            View::Album(album) => ScreenBuilder::new("pharos-album")
                .top_bar(album.name.clone())
                .rows(Self::rows(&self.tracks, self.page, true))
                .page_turns("page-back", "page-next")
                .build()
                .with_own_back(true),
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

    fn show(&self, context: &mut Context) {
        let screen = self.screen(context);
        context.set_screen(screen);
    }

    fn row_index(action: ActionId, visible: usize) -> Option<usize> {
        (0..visible).find(|index| action == action_id(&format!("row-{index}")))
    }

    fn turn(&mut self, context: &mut Context, forward: bool) {
        let page = match &self.view {
            View::Albums => &self.albums,
            View::Album(_) => &self.tracks,
            _ => return,
        };
        let pages = page.entries.len().div_ceil(ROWS_PER_PAGE).max(1);
        if forward && self.page + 1 < pages {
            self.page += 1;
        } else if !forward && self.page > 0 {
            self.page -= 1;
        }
        // Fetch the next block of albums before the reader reaches its end.
        let loaded = self.albums.entries.len();
        if matches!(self.view, View::Albums)
            && loaded < self.albums.total
            && (self.page + 2) * ROWS_PER_PAGE >= loaded
            && !self
                .tasks
                .iter()
                .any(|(_, work)| matches!(work, Work::Albums { .. }))
        {
            self.load_albums(context, loaded);
        }
    }
}

fn work_label(work: &Work) -> &'static str {
    match work {
        Work::Initiate => "initiate",
        Work::Wait { .. } => "wait",
        Work::Poll { .. } => "poll",
        Work::Finish => "finish",
        Work::Albums { .. } => "albums",
        Work::Tracks => "tracks",
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
                    self.view = View::Albums;
                    self.load_albums(context, 0);
                }
                None => self.sign_in(context),
            }
            self.show(context);
        }
    }

    fn on_action(&mut self, context: &mut Context, action: ActionId) {
        if matches!(self.view, View::Player) {
            if action == ActionId::BACK {
                // Backing out keeps the track playing, as the player says.
                self.view = self.album.clone().map_or(View::Albums, View::Album);
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
            if let View::Album(_) = self.view {
                self.view = View::Albums;
                self.album = None;
                self.page = 0;
            }
        } else if action == action_id("retry") {
            match self.account {
                Some(_) => {
                    self.view = View::Albums;
                    self.load_albums(context, 0);
                }
                None => self.sign_in(context),
            }
        } else if action == action_id("sign-out") {
            self.sign_out(context);
        } else if action == action_id("page-next") {
            self.turn(context, true);
        } else if action == action_id("page-back") {
            self.turn(context, false);
        } else {
            let visible = match &self.view {
                View::Albums => self.albums.entries.len(),
                View::Album(_) => self.tracks.entries.len(),
                _ => 0,
            };
            if let Some(index) = Self::row_index(action, visible) {
                match &self.view {
                    View::Albums => {
                        if let Some(album) = self.albums.entries.get(index).cloned() {
                            self.open_album(context, album);
                        }
                    }
                    View::Album(_) => self.play(context, index),
                    _ => {}
                }
            }
        }
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
        let Some(position) = self.tasks.iter().position(|(id, _)| *id == task) else {
            return;
        };
        let (_, work) = self.tasks.swap_remove(position);
        if self.report_timer == Some(task) {
            self.report_timer = None;
        }
        match outcome {
            TaskOutcome::Completed(bytes) => self.completed(context, work, &bytes),
            TaskOutcome::Failed(error) => self.failed(context, work, error),
            TaskOutcome::Cancelled => {}
        }
        self.show(context);
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
