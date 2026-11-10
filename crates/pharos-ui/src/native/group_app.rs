//! The watch-together store as the app runs it: the browser's clock, timers
//! and `<video>` behind [`Session`]'s traits, and the handle the screens share.

use std::rc::Rc;

use dioxus::prelude::*;
use futures_util::future::LocalBoxFuture;
use futures_util::FutureExt as _;
use pharos_client::api::{ClientEvent, ItemRef, NativeApi, Track};

use super::activity::TrackKind;
use super::group_session::{Env, GroupPlayer, GroupView, Seat, Session, Ui};
use super::platform;
use super::together::{group_api, Groups};
use super::track_follow::{TrackMap, TrackPicks};

/// The one session type the app uses.
pub type AppSession = Session<Groups, AppEnv>;

/// [`Env`] over the browser. Store updates for the screens go through a
/// coroutine, because the server's pushes arrive outside the Dioxus runtime.
#[derive(Clone)]
pub struct AppEnv {
    ui: Coroutine<Ui>,
}

impl Env for AppEnv {
    fn now_ms(&self) -> i64 {
        platform::unix_ms()
    }

    fn sleep(&self, ms: u32) -> LocalBoxFuture<'static, ()> {
        platform::sleep_ms(ms).boxed_local()
    }

    fn spawn(&self, fut: LocalBoxFuture<'static, ()>) {
        platform::spawn_detached(fut);
    }

    fn report(&self, event: ClientEvent) {
        platform::report_event(event);
    }

    fn ui(&self, ui: Ui) {
        self.ui.send(ui);
    }

    fn info(&self, message: &str) {
        platform::info(message);
    }

    fn warn(&self, message: &str) {
        platform::warn(message);
    }
}

/// The page's `<video>` as the group drives it.
pub struct VideoPlayer {
    pub id: &'static str,
    /// The item's audio and subtitle streams.
    pub audio: Vec<Track>,
    pub subtitles: Vec<Track>,
    /// The group names the primary file's streams; this lines the version
    /// playing up with them (ADR-0089).
    pub map: TrackMap,
    /// The version playing, for the logs (empty: the primary).
    pub version: String,
    /// Where the group's track picks go: the player screen switches tracks
    /// there, inside the Dioxus runtime.
    pub tracks: Coroutine<TrackPicks>,
}

impl GroupPlayer for VideoPlayer {
    fn position_ms(&self) -> u64 {
        platform::position_ms(self.id)
    }

    fn paused(&self) -> bool {
        platform::media_state(self.id).paused
    }

    fn ended(&self) -> bool {
        platform::ended(self.id)
    }

    fn has_data(&self) -> bool {
        platform::has_data(self.id)
    }

    fn play(&self) {
        platform::play(self.id);
    }

    fn autoplay_blocked(&self) -> bool {
        platform::autoplay_blocked()
    }

    fn pause(&self) {
        platform::pause(self.id);
    }

    fn seek_ms(&self, ms: u64) {
        platform::seek_ms(self.id, ms);
    }

    fn set_rate(&self, rate: f64) {
        platform::set_rate(self.id, rate);
    }

    /// As the primary's indexes: the group's picks name those.
    fn stream_indexes(&self) -> (Vec<u32>, Vec<u32>) {
        (
            self.map.primary_indexes(TrackKind::Audio),
            self.map.primary_indexes(TrackKind::Subtitles),
        )
    }

    fn track_label(&self, kind: TrackKind, index: u32) -> Option<String> {
        let tracks = match kind {
            TrackKind::Audio => &self.audio,
            TrackKind::Subtitles => &self.subtitles,
        };
        let own = self.map.to_own(kind, index)?;
        let track = tracks.iter().find(|t| t.index == own)?;
        [&track.title, &track.language]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned()
    }

    fn apply_tracks(&self, picks: TrackPicks) {
        let own = self.map.picks_to_own(picks);
        let dropped = [
            (
                TrackKind::Audio,
                picks.audio.filter(|_| own.audio.is_none()),
            ),
            (
                TrackKind::Subtitles,
                picks.subtitle.flatten().filter(|_| own.subtitle.is_none()),
            ),
        ];
        for (kind, index) in dropped {
            if let Some(index) = index {
                platform::info(&format!(
                    "pharos: group tracks unmapped kind={kind:?} index={index} version={:?}",
                    self.version
                ));
            }
        }
        if own.audio.is_some() || own.subtitle.is_some() {
            self.tracks.send(own);
        }
    }
}

/// What every screen shares about watching together.
#[derive(Clone)]
pub struct Together {
    /// The group as the viewer sees it; reading it subscribes the caller.
    pub view: Signal<GroupView>,
    /// Why the last create or join failed. It lives with the app, not a
    /// screen: the attempt outlasts the screen that started it.
    pub error: Signal<Option<String>>,
    ui: Coroutine<Ui>,
    seat: Rc<Seat<Groups, AppEnv>>,
}

impl Together {
    pub fn new(view: Signal<GroupView>, error: Signal<Option<String>>, ui: Coroutine<Ui>) -> Self {
        Self {
            view,
            error,
            ui,
            seat: Rc::new(Seat::new(platform::info)),
        }
    }

    /// The session to create or join a group with. One in a group is kept;
    /// otherwise a new one is made over the current connection, so a
    /// reconnect never leaves a session on a dead socket. It drives the
    /// stage's video, if one plays.
    pub fn session(&self, api: &NativeApi) -> Rc<AppSession> {
        self.seat
            .session(|| Session::new(group_api(api), AppEnv { ui: self.ui }))
    }

    /// The app's connection was replaced by `api`'s, as after a deploy or a
    /// dropped socket. A session in a group moves to it and joins its group
    /// again: its membership lived on the old connection, so without this the
    /// tab would think itself in the group while hearing nothing from it.
    pub fn reconnected(&self, api: &NativeApi) {
        let Some(session) = self.seat.current().filter(|s| s.active()) else {
            return;
        };
        session.client().reconnect(api);
        platform::spawn_detached(Box::pin(async move {
            session.rejoin("reconnect").await;
        }));
    }

    /// A video is on the stage: the group drives it, whatever route shows.
    pub fn attach(&self, item: ItemRef, player: Rc<dyn GroupPlayer>) {
        self.seat.attach(item, player);
    }

    /// The video is gone from the stage.
    pub fn detach(&self, player: &Rc<dyn GroupPlayer>) {
        self.seat.detach(player);
    }

    /// The session the watch page drives, if there is one.
    pub fn current(&self) -> Option<Rc<AppSession>> {
        self.seat.current()
    }
}

/// Where a tab keeps the group it is in, so a reload joins it again (B665).
/// `sessionStorage`, like the tab's group device id (`pharos.group.device`):
/// the server keeps a reloaded member's seat for the socket-loss grace, and
/// the same device joining takes it back.
const GROUP_KEY: &str = "pharos.group.id";

/// What the tab keeps for `view`: the group's id while in one, else empty.
fn kept_group(view: &GroupView) -> String {
    if view.active {
        view.id.clone().unwrap_or_default()
    } else {
        String::new()
    }
}

/// Keeps the tab's group for a reload, writing only when it changes.
#[derive(Default)]
pub struct GroupKeeper(std::cell::RefCell<Option<String>>);

impl GroupKeeper {
    pub fn keep(&self, view: &GroupView) {
        let id = kept_group(view);
        if self.0.borrow().as_deref() == Some(id.as_str()) {
            return;
        }
        if let Err(e) = platform::session_set(GROUP_KEY, &id) {
            platform::warn(&format!(
                "pharos: keeping the group for a reload failed: {e}"
            ));
        }
        *self.0.borrow_mut() = Some(id);
    }
}

/// The group this tab was in before it was reloaded, to join again; `None`
/// when it was in none, or is in one already.
pub fn group_to_rejoin(kept: Option<String>, in_group: bool) -> Option<String> {
    kept.filter(|id| !id.is_empty() && !in_group)
}

pub use super::group_session::rejoin_failure;

/// What the tab kept before this page loaded.
pub fn kept_before_load() -> Option<String> {
    platform::session_get(GROUP_KEY).unwrap_or_else(|e| {
        platform::warn(&format!("pharos: reading the kept group failed: {e}"));
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_keeps_its_group_only_while_in_it() {
        let mut view = GroupView {
            active: true,
            id: Some("g1".into()),
            ..GroupView::default()
        };
        assert_eq!(kept_group(&view), "g1");
        view.active = false;
        assert_eq!(
            kept_group(&view),
            "",
            "a left or ended group is not rejoined"
        );
    }

    #[test]
    fn a_failed_rejoin_says_so_and_carries_the_reason() {
        assert_eq!(
            rejoin_failure("This group has ended"),
            "Couldn't rejoin your group, so you're watching on your own. (This group has ended)"
        );
    }

    #[test]
    fn a_reload_rejoins_the_kept_group_unless_already_in_one() {
        assert_eq!(
            group_to_rejoin(Some("g1".into()), false).as_deref(),
            Some("g1")
        );
        assert_eq!(group_to_rejoin(Some("g1".into()), true), None);
        assert_eq!(group_to_rejoin(Some(String::new()), false), None);
        assert_eq!(group_to_rejoin(None, false), None);
    }
}
