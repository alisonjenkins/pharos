//! The watch-together store as the app runs it: the browser's clock, timers
//! and `<video>` behind [`Session`]'s traits, and the handle the screens share.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use futures_util::future::LocalBoxFuture;
use futures_util::FutureExt as _;
use pharos_client::api::{ClientEvent, NativeApi};

use super::group_session::{Env, GroupPlayer, GroupView, Session, Ui};
use super::platform;
use super::together::{group_api, Groups};

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

    fn warn(&self, message: &str) {
        platform::warn(message);
    }
}

/// The page's `<video>` as the group drives it.
pub struct VideoPlayer(pub &'static str);

impl GroupPlayer for VideoPlayer {
    fn position_ms(&self) -> u64 {
        platform::position_ms(self.0)
    }

    fn paused(&self) -> bool {
        platform::media_state(self.0).paused
    }

    fn play(&self) {
        platform::play(self.0);
    }

    fn pause(&self) {
        platform::pause(self.0);
    }

    fn seek_ms(&self, ms: u64) {
        platform::seek_ms(self.0, ms);
    }

    fn set_rate(&self, rate: f64) {
        platform::set_rate(self.0, rate);
    }
}

/// What every screen shares about watching together.
#[derive(Clone)]
pub struct Together {
    /// The group as the viewer sees it; reading it subscribes the caller.
    pub view: Signal<GroupView>,
    ui: Coroutine<Ui>,
    current: Rc<RefCell<Option<Rc<AppSession>>>>,
}

impl Together {
    pub fn new(view: Signal<GroupView>, ui: Coroutine<Ui>) -> Self {
        Self {
            view,
            ui,
            current: Rc::default(),
        }
    }

    /// The session to create or join a group with. One in a group is kept;
    /// otherwise a new one is made over the current connection, so a
    /// reconnect never leaves a session on a dead socket. It takes over the
    /// old one's player and watched item, which the watch page set once.
    pub fn session(&self, api: &NativeApi) -> Rc<AppSession> {
        let mut current = self.current.borrow_mut();
        if let Some(session) = current.as_ref().filter(|s| s.active()) {
            return Rc::clone(session);
        }
        let session = Session::new(group_api(api), AppEnv { ui: self.ui });
        if let Some(old) = current.as_ref() {
            session.adopt(old);
        }
        *current = Some(Rc::clone(&session));
        session
    }

    /// The session the watch page drives, if there is one.
    pub fn current(&self) -> Option<Rc<AppSession>> {
        self.current.borrow().clone()
    }
}
