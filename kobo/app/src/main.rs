//! Pharos on a Kobo (spec 047, ADR-0136).
//!
//! Signs in to the owner's pharos with a Quick Connect code, lists the music
//! albums, and plays a track through the runtime's Bluetooth audio, telling
//! pharos where the listener is so any other device can carry on.
//!
//! The server address is fixed when the package is built
//! (`PHAROS_KOBO_SERVER`); the reader has one owner and one server.

mod api;
mod offline;

use api::{Account, Entry, Page, Report};
use kobo_sdk::audio::{AudioMetadata, AudioPlayer};
use kobo_sdk::prelude::*;
use kobo_sdk::{Header, LogLevel, Task, TaskError, TaskId, TaskOutcome};
use offline::{Offline, Unsent};
use std::collections::HashMap;
use std::process::ExitCode;

const SERVER: &str = match option_env!("PHAROS_KOBO_SERVER") {
    Some(server) => server,
    None => "https://pharos.test",
};
const VERSION: &str = env!("CARGO_PKG_VERSION");
const ACCOUNT_KEY: &str = "account";
/// The albums with downloads and their tracks, for listing with no server.
const OFFLINE_KEY: &str = "offline";
/// Places the listener reached that the server has not heard yet.
const UNSENT_KEY: &str = "unsent";
const POLL_SECONDS: u32 = 3;
/// How often a playing track's place is sent while nobody touches anything.
const REPORT_SECONDS: u32 = 30;
/// How often the downloads of the open album are asked after, so the marks
/// and percentages follow a download that is still running.
const DOWNLOAD_POLL_SECONDS: u32 = 10;
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
    Wait {
        secret: String,
    },
    Poll {
        secret: String,
    },
    Finish,
    Albums {
        start: usize,
    },
    /// One block of an album's tracks, from `start`.
    Tracks {
        album: String,
        start: usize,
    },
    /// A playing-session call, with the place it was made at so a failure can
    /// keep that place for later.
    Report {
        report: Report,
        track: String,
        position_ms: u64,
    },
    ReportTimer,
    /// A place kept from when the server was out of reach, sent now.
    Replay {
        track: String,
    },
    DownloadsTimer,
}

/// How much of a track is on the reader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Kept {
    have: u64,
    total: u64,
}

impl Kept {
    fn label(self) -> String {
        if self.have >= self.total {
            "Downloaded".to_owned()
        } else {
            let percent = self.have.saturating_mul(100) / self.total.max(1);
            format!("{percent}%")
        }
    }
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
    /// The albums with downloads, as last saved.
    offline: Offline,
    /// The albums on screen are the saved ones because the server did not
    /// answer.
    offline_mode: bool,
    /// Places not yet sent to the server.
    unsent: Unsent,
    /// What is on the reader, by track id, as of the last answer.
    downloads: HashMap<String, Kept>,
    /// The addresses of the last question, with the track each stands for.
    asked: Vec<(String, String)>,
    downloads_timer: Option<TaskId>,
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
            offline: Offline::default(),
            offline_mode: false,
            unsent: Unsent::default(),
            downloads: HashMap::new(),
            asked: Vec::new(),
            downloads_timer: None,
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
        // Another account's library and places are not this one's.
        self.offline = Offline::default();
        self.unsent = Unsent::default();
        self.offline_mode = false;
        self.downloads.clear();
        context.store().forget(OFFLINE_KEY);
        context.store().forget(UNSENT_KEY);
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
        if self.account.is_none() {
            return;
        }
        self.tracks = Page::default();
        self.page = 0;
        self.album = Some(album.clone());
        match self
            .offline_mode
            .then(|| self.offline.tracks(&album.id))
            .flatten()
        {
            Some(saved) => self.tracks = saved,
            None => self.load_tracks(context, album.id.clone(), 0),
        }
        self.view = View::Album(album);
        self.refresh_downloads(context);
        self.arm_downloads_timer(context);
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
        // Playing is what starts the download; ask how it is getting on.
        self.refresh_downloads(context);
        self.arm_downloads_timer(context);
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
        let work = Work::Report {
            report,
            track: playing.track.clone(),
            position_ms,
        };
        self.post(context, report.url(SERVER), body, work);
    }

    /// Sends the places kept from when the server was out of reach, as the
    /// end of a session each. The track playing now is left: its own reports
    /// are newer than anything kept.
    fn replay_unsent(&mut self, context: &mut Context) {
        let playing = self.playing.as_ref().map(|playing| playing.track.clone());
        let waiting: Vec<(String, u64)> = self
            .unsent
            .places()
            .iter()
            .filter(|(track, _)| Some(track) != playing.as_ref())
            .cloned()
            .collect();
        for (track, position_ms) in waiting {
            let sending = self
                .tasks
                .iter()
                .any(|(_, work)| matches!(work, Work::Replay { track: sent } if *sent == track));
            if sending {
                continue;
            }
            let body = api::report_body(&track, position_ms, true);
            self.post(
                context,
                Report::Stopped.url(SERVER),
                body,
                Work::Replay { track },
            );
        }
    }

    fn save_unsent(&self, context: &mut Context) {
        if self.unsent.is_empty() {
            context.store().forget(UNSENT_KEY);
        } else {
            context.store().save(UNSENT_KEY, self.unsent.encode());
        }
    }

    /// The tracks on screen whose downloads are worth asking after: from the
    /// panel page on, and the one playing.
    fn download_questions(&self) -> Vec<(String, String)> {
        let Some(account) = &self.account else {
            return Vec::new();
        };
        let mut tracks: Vec<&str> = self
            .tracks
            .entries
            .iter()
            .skip(self.page * ROWS_PER_PAGE)
            .take(kobo_sdk::MAX_DOWNLOAD_URLS)
            .map(|entry| entry.id.as_str())
            .collect();
        if let Some(playing) = &self.playing {
            if !tracks.contains(&playing.track.as_str()) {
                tracks.pop();
                tracks.push(playing.track.as_str());
            }
        }
        tracks
            .into_iter()
            .map(|id| (api::stream_url(SERVER, account, id), id.to_owned()))
            .collect()
    }

    fn refresh_downloads(&mut self, context: &mut Context) {
        let questions = self.download_questions();
        if questions.is_empty() {
            return;
        }
        let urls = questions.iter().map(|(url, _)| url.clone()).collect();
        if context.device().list_downloads(urls) {
            self.asked = questions;
        }
    }

    fn arm_downloads_timer(&mut self, context: &mut Context) {
        if self.downloads_timer.is_none() {
            self.downloads_timer = self.sleep(context, DOWNLOAD_POLL_SECONDS, Work::DownloadsTimer);
        }
    }

    /// Notes what the runtime says is on the reader and keeps the album's
    /// names for listing with no server while any of it is.
    fn heard_downloads(&mut self, context: &mut Context, entries: &[kobo_sdk::DownloadEntry]) {
        let asked = std::mem::take(&mut self.asked);
        for (_, track) in &asked {
            self.downloads.remove(track);
        }
        for entry in entries {
            if let Some((_, track)) = asked.iter().find(|(url, _)| *url == entry.url) {
                self.downloads.insert(
                    track.clone(),
                    Kept {
                        have: entry.have,
                        total: entry.total,
                    },
                );
            }
        }
        self.asked = asked;
        self.save_offline(context);
    }

    fn save_offline(&mut self, context: &mut Context) {
        let Some(album) = self.album.clone() else {
            return;
        };
        // Only a whole album says anything about what is kept of it.
        if self.tracks.entries.is_empty() || self.tracks.entries.len() < self.tracks.total {
            return;
        }
        let any = self.tracks.entries.iter().any(|track| {
            self.downloads
                .get(&track.id)
                .is_some_and(|kept| kept.have > 0)
        });
        let checked_all = self.asked.len() >= self.tracks.entries.len();
        let changed = if any {
            self.offline.keep(&album, &self.tracks.entries)
        } else {
            checked_all && self.offline.forget(&album.id)
        };
        if changed {
            if self.offline.is_empty() {
                context.store().forget(OFFLINE_KEY);
            } else {
                context.store().save(OFFLINE_KEY, self.offline.encode());
            }
        }
    }

    /// The server did not answer, but albums with downloads are saved: list
    /// those.
    fn go_offline(&mut self) {
        self.offline_mode = true;
        self.albums = self.offline.page();
        self.page = 0;
        self.view = View::Albums;
    }

    fn remove_downloads(&mut self, context: &mut Context) {
        let Some(account) = &self.account else {
            return;
        };
        let urls: Vec<String> = self
            .tracks
            .entries
            .iter()
            .filter(|track| {
                self.downloads
                    .get(&track.id)
                    .is_some_and(|kept| kept.have > 0)
            })
            .map(|track| api::stream_url(SERVER, account, &track.id))
            .collect();
        for url in urls {
            let _queued = context.device().remove_download(url);
        }
        self.refresh_downloads(context);
    }

    #[allow(clippy::too_many_lines)]
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
                    self.offline_mode = false;
                    self.replay_unsent(context);
                    if start == 0 {
                        self.albums = page;
                    } else {
                        self.albums.entries.extend(page.entries);
                        self.albums.total = page.total;
                    }
                }
                None => self.problem("Pharos sent a list this app could not read."),
            },
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
                        } else {
                            self.refresh_downloads(context);
                        }
                    }
                    None => self.problem("Pharos sent a list this app could not read."),
                }
            }
            Work::Report {
                report,
                track,
                position_ms,
            } => {
                context.log(
                    LogLevel::Info,
                    format!("pharos: report {} sent", report.label()),
                );
                // The server hears this track again, so a kept place for it
                // is stale; the others are news to it.
                if self.unsent.sent(&track) {
                    self.save_unsent(context);
                }
                let _ = position_ms;
                self.replay_unsent(context);
            }
            Work::Replay { track } => {
                context.log(
                    LogLevel::Info,
                    format!("pharos: kept place sent track={track}"),
                );
                if self.unsent.sent(&track) {
                    self.save_unsent(context);
                }
            }
            Work::DownloadsTimer => {
                self.downloads_timer = None;
                if matches!(self.view, View::Album(_) | View::Player) {
                    self.refresh_downloads(context);
                    self.arm_downloads_timer(context);
                }
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
            (_, TaskError::Unauthorized) if self.account.is_some() => {
                // Signed out on the server (the device removed, or the
                // password changed): forget the token and sign in again.
                self.sign_out(context);
            }
            // A report the server did not hear keeps the place, to send when it
            // answers again. The next one carries a newer place anyway.
            (
                Work::Report {
                    track, position_ms, ..
                },
                _,
            ) => {
                self.unsent.note(&track, position_ms);
                self.save_unsent(context);
            }
            (Work::ReportTimer | Work::DownloadsTimer | Work::Replay { .. }, _) => {}
            // Albums with downloads are still there with the server away.
            (
                Work::Albums { start: 0 },
                TaskError::Offline | TaskError::Unreachable | TaskError::TimedOut,
            ) if !self.offline.is_empty() => self.go_offline(),
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

    /// The rows of `page` on the panel page `self.page`. `tracks` says they are
    /// tracks, which show their length and how much of them is on the reader;
    /// an album with any of its tracks saved says so.
    fn rows(&self, page: &Page, tracks: bool) -> Vec<(String, String, String, u16)> {
        page.entries
            .iter()
            .enumerate()
            .skip(self.page * ROWS_PER_PAGE)
            .take(ROWS_PER_PAGE)
            .map(|(index, entry)| {
                let mut summary = entry.by.clone();
                let mut add = |part: &str| {
                    if !summary.is_empty() {
                        summary.push_str(" · ");
                    }
                    summary.push_str(part);
                };
                if let (true, Some(ms)) = (tracks, entry.runtime_ms) {
                    add(&api::clock(ms));
                }
                if tracks {
                    if let Some(kept) = self.downloads.get(&entry.id).filter(|kept| kept.have > 0) {
                        add(&kept.label());
                    }
                } else if self.offline.contains(&entry.id) {
                    add("Downloaded");
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
            View::Albums if self.albums.entries.is_empty() => self
                .now_playing(ScreenBuilder::new("pharos-albums").top_bar("Music"))
                .activity("Loading your albums", None)
                .button("sign-out", "Sign out")
                .build(),
            View::Albums if self.offline_mode => self
                .now_playing(ScreenBuilder::new("pharos-albums").top_bar(format!(
                    "Downloaded · {} albums · offline",
                    self.albums.total
                )))
                .rows(self.rows(&self.albums, false))
                .page_turns("page-back", "page-next")
                .bottom_action("retry", "Try the server again")
                .build(),
            View::Albums => self
                .now_playing(
                    ScreenBuilder::new("pharos-albums")
                        .top_bar(format!("Music · {} albums", self.albums.total)),
                )
                .rows(self.rows(&self.albums, false))
                .page_turns("page-back", "page-next")
                .bottom_action("sign-out", "Sign out")
                .build(),
            View::Album(album) if self.tracks.entries.is_empty() => self
                .now_playing(ScreenBuilder::new("pharos-album").top_bar(album.name.clone()))
                .activity("Loading the tracks", None)
                .build()
                .with_own_back(true),
            View::Album(album) => {
                let screen = self
                    .now_playing(ScreenBuilder::new("pharos-album").top_bar(album.name.clone()))
                    .rows(self.rows(&self.tracks, true))
                    .page_turns("page-back", "page-next");
                let kept = self.tracks.entries.iter().any(|track| {
                    self.downloads
                        .get(&track.id)
                        .is_some_and(|kept| kept.have > 0)
                });
                if kept {
                    screen.bottom_action("remove-downloads", "Remove downloads")
                } else {
                    screen
                }
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

    /// Adds the way back to the player while something plays: Back from the
    /// player keeps the track going, and choosing it again would restart it.
    fn now_playing(&self, screen: ScreenBuilder) -> ScreenBuilder {
        if self.playing.is_some() {
            screen.top_bar_action("now-playing", "Playing")
        } else {
            screen
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
        Work::Tracks { .. } => "tracks",
        Work::Report { report, .. } => report.label(),
        Work::ReportTimer => "report-timer",
        Work::Replay { .. } => "replay",
        Work::DownloadsTimer => "downloads-timer",
    }
}

impl KoboApp for Pharos {
    fn on_start(&mut self, context: &mut Context) {
        // The account last: answers come in order, and a server that does
        // not answer must find the saved albums already read.
        context.store().load(OFFLINE_KEY);
        context.store().load(UNSENT_KEY);
        context.store().load(ACCOUNT_KEY);
        self.show(context);
    }

    fn on_store(&mut self, context: &mut Context, result: StoreResult) {
        if let StoreResult::Loaded { key, value } = result {
            if key == OFFLINE_KEY {
                self.offline = value.as_deref().map(Offline::decode).unwrap_or_default();
                return;
            }
            if key == UNSENT_KEY {
                self.unsent = value.as_deref().map(Unsent::decode).unwrap_or_default();
                return;
            }
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
        } else if action == action_id("remove-downloads") {
            self.remove_downloads(context);
        } else if action == action_id("retry") {
            match self.account {
                Some(_) => {
                    self.offline_mode = false;
                    self.view = View::Albums;
                    self.load_albums(context, 0);
                }
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
        match (&request, &result) {
            (DeviceRequest::ListDownloads { .. }, DeviceResult::Downloads { entries }) => {
                self.heard_downloads(context, entries);
                if matches!(self.view, View::Album(_)) {
                    self.show(context);
                }
                return;
            }
            (DeviceRequest::ListDownloads { .. } | DeviceRequest::RemoveDownload { .. }, other) => {
                context.log(
                    LogLevel::Info,
                    format!("pharos: downloads call answered {other:?}"),
                );
                return;
            }
            _ => {}
        }
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
        if self.downloads_timer == Some(task) {
            self.downloads_timer = None;
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
