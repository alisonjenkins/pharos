//! The native app: routing and history, the connection lifecycle that
//! drives [`Machine`], and the screens that load data for the pure views.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use futures_util::StreamExt as _;
use pharos_client::api::{
    Detail, Home, ItemRef, NativeApi, NativeGrant, Page, PharosApi as _, PlaybackGrant as _,
};
use pharos_client::{CallInfo, ClientError};

use super::machine::{Event, Machine, Phase};
use super::platform::{self, OpenError};
use super::playback::{
    choose_engine, start_ms, Engine, MediaEvent, OnFatal, Outgoing, PlaybackEngine, ReportClock,
};
use super::route::Route;
use super::views::{
    BrowseView, ErrorNote, HomeView, Loading, Nav, Shell, SignInView, StatusView, TitleView,
    WatchView, VIDEO_ID,
};
use crate::theme;
use crate::views::RootApp;

/// Items a browse page asks for at once.
const BROWSE_PAGE: u32 = 100;

/// The page root: one route per URL, kept in step with history. `/manage`
/// mounts the older REST panes and never opens the native socket.
#[component]
pub fn Root() -> Element {
    use_hook(apply_device_theme);
    let mut route = use_signal(|| Route::parse(&platform::current_path()));
    use_hook(|| {
        Rc::new(platform::on_pop_state(move |path| {
            route.set(Route::parse(&path))
        }))
    });
    use_context_provider(|| {
        Nav(EventHandler::new(move |to: Route| {
            if *route.peek() != to {
                platform::push(&to.path());
                route.set(to);
            }
        }))
    });
    use_context_provider(|| {
        Replace(EventHandler::new(move |to: Route| {
            platform::replace(&to.path());
            route.set(to);
        }))
    });
    match route() {
        Route::Manage => rsx! { RootApp {} },
        route => rsx! { NativeApp { route } },
    }
}

/// Apply the device's stored theme before the first native paint, logging
/// which theme and, on a fallback, why.
fn apply_device_theme() {
    let stored = platform::stored_theme(theme::device::ACTIVE_KEY);
    let startup = match theme::device::startup(&theme::shipped(), stored) {
        Ok(startup) => startup,
        Err(e) => {
            platform::warn(&format!(
                "pharos: theme unavailable reason={} keeping=pharos.css",
                e.label()
            ));
            return;
        }
    };
    for line in &startup.lines {
        match line {
            theme::device::Line::Info(l) => platform::info(l),
            theme::device::Line::Warn(l) => platform::warn(l),
        }
    }
    if let Err(e) = platform::apply_tokens(&theme::css::declarations(&startup.theme)) {
        platform::warn(&format!(
            "pharos: theme apply failed name={} error={e}",
            startup.theme.name
        ));
    }
}

/// Navigation that replaces the current history entry, for redirects.
#[derive(Clone, Copy)]
struct Replace(EventHandler<Route>);

/// What screens share: the signed-in API (none until resumed) and the
/// machine, so a `revoked` answer can send the app to sign-in.
#[derive(Clone, Copy)]
struct Ctx {
    api: Signal<Option<NativeApi>>,
    machine: Signal<Machine>,
}

#[component]
fn NativeApp(route: Route) -> Element {
    let mut machine = use_signal(Machine::new);
    let mut api = use_signal(|| None::<NativeApi>);
    let mut task = use_signal(|| None::<dioxus::core::Task>);
    let mut started = use_signal(|| None::<u32>);
    use_context_provider(|| Ctx { api, machine });
    let nav = use_context::<Nav>();

    use_effect(move || {
        let (phase, epoch, delay) = {
            let m = machine.read();
            (m.phase().clone(), m.epoch(), m.delay_ms())
        };
        match phase {
            Phase::Connecting if *started.peek() != Some(epoch) => {
                started.set(Some(epoch));
                if let Some(old) = task.write().take() {
                    old.cancel();
                }
                // A reconnect keeps the old API until the new one resumes,
                // so the screens keep what they loaded.
                task.set(Some(spawn(connect(machine, api, epoch, delay))));
            }
            Phase::Connecting | Phase::Ready => {}
            _ => {
                if let Some(old) = task.write().take() {
                    old.cancel();
                }
                if api.peek().is_some() {
                    api.set(None);
                }
            }
        }
    });

    // Ready and reconnecting render through this one node, so a dropped
    // socket does not remount the screen (and its playing video).
    if machine.read().shows_screens() {
        let notice = {
            let m = machine.read();
            if m.reconnecting() {
                Some("Connection lost. Reconnecting…".to_owned())
            } else {
                m.notice().map(str::to_owned)
            }
        };
        return rsx! {
            Shell {
                on_sign_out: move |_| {
                    if machine.peek().signing_out() {
                        return;
                    }
                    machine.write().apply(Event::SignOutSubmitted);
                    spawn(async move {
                        let reply = platform::sign_out().await;
                        machine.write().apply(Event::SignOutAnswered(reply));
                    });
                },
                if let Some(message) = notice {
                    ErrorNote { message }
                }
                Screen { route }
            }
        };
    }
    let phase = machine.read().phase().clone();
    match phase {
        Phase::Connecting | Phase::Ready => rsx! {
            StatusView {
                title: "Connecting…",
                detail: "Opening a connection to the server.",
                action: None,
            }
        },
        Phase::NotEnabled => rsx! {
            StatusView {
                title: "This interface is not enabled",
                detail: "The server has the native API turned off \
                         ([server.native_api].enabled). The existing screens are under Manage.",
                action: Some(("Open Manage".to_string(), EventHandler::new(move |_| nav.0.call(Route::Manage)))),
            }
        },
        Phase::Unreachable { detail } => rsx! {
            StatusView {
                title: "Cannot connect",
                detail,
                action: Some(("Try again".to_string(), EventHandler::new(move |_| machine.write().apply(Event::Retry)))),
            }
        },
        Phase::SignIn { error, busy } => rsx! {
            SignInView {
                error,
                busy,
                on_submit: move |(name, password): (String, String)| {
                    machine.write().apply(Event::SignInSubmitted);
                    spawn(async move {
                        let reply = platform::sign_in(&name, &password).await;
                        machine.write().apply(Event::SignInAnswered(reply));
                    });
                },
            }
        },
    }
}

/// One connection attempt: wait out the backoff, open and resume, then
/// report the socket's close.
async fn connect(
    mut machine: Signal<Machine>,
    mut api: Signal<Option<NativeApi>>,
    epoch: u32,
    delay_ms: u32,
) {
    platform::sleep_ms(delay_ms).await;
    let event = match platform::open(&call("connect", "resume")).await {
        Ok(mut open) => {
            api.set(Some(open.api.clone()));
            machine.write().apply(Event::Resumed { epoch });
            let reason = platform::closed(&mut open.conn).await;
            Event::Closed { epoch, reason }
        }
        Err(OpenError::NotOpened(probe)) => Event::NotOpened { epoch, probe },
        Err(OpenError::NotSignedIn) => Event::ResumeRefused { epoch },
        Err(OpenError::Resume(error)) => Event::ResumeFailed { epoch, error },
    };
    machine.write().apply(event);
}

fn call(route: &str, action: &str) -> CallInfo {
    CallInfo {
        ui_route: route.into(),
        ui_action: action.into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        device: "web".into(),
        ..CallInfo::default()
    }
}

/// A finished load: the value, or the message to show. A `revoked` answer
/// also sends the app to sign-in.
fn settle<T>(mut machine: Signal<Machine>, result: Result<T, ClientError>) -> Result<T, String> {
    result.map_err(|e| {
        if e.needs_sign_in() {
            machine.write().apply(Event::NeedsSignIn);
        }
        e.to_string()
    })
}

#[component]
fn Screen(route: Route) -> Element {
    match route {
        Route::Home => rsx! { HomeScreen {} },
        Route::Browse(id) => rsx! { BrowseScreen { id } },
        Route::Title(id) => rsx! { TitleScreen { id } },
        Route::Watch(id) => rsx! { WatchScreen { id } },
        Route::SignIn => rsx! { Redirect { to: Route::Home } },
        // `Root` mounts the older panes for this route.
        Route::Manage => rsx! { Redirect { to: Route::Manage } },
        Route::NotFound(path) => rsx! { ErrorNote { message: format!("Nothing at {path}.") } },
    }
}

/// Replace the current route once mounted (signed in at `/signin`).
#[component]
fn Redirect(to: Route) -> Element {
    let replace = use_context::<Replace>();
    use_effect(move || replace.0.call(to.clone()));
    rsx! { Loading {} }
}

/// Renders a load's state: loading, its error, or `view` of its value.
fn loaded<T>(state: Option<&Result<T, String>>, view: impl FnOnce(&T) -> Element) -> Element {
    match state {
        None => rsx! { Loading {} },
        Some(Err(message)) => rsx! { ErrorNote { message: message.clone() } },
        Some(Ok(value)) => view(value),
    }
}

#[component]
fn HomeScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let home = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            Some(settle(ctx.machine, api.home(&call("home", "load")).await))
        }
    });
    let state = home.read();
    loaded(state.as_ref().and_then(Option::as_ref), |home: &Home| {
        rsx! { HomeView { home: home.clone() } }
    })
}

#[component]
fn BrowseScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    let listing = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            let info = call("browse", "load");
            let result = async {
                let detail = api.detail(&info, id).await?;
                let page = api.browse(&info, id, 0, BROWSE_PAGE).await?;
                Ok::<_, ClientError>((detail.item.name, page))
            }
            .await;
            Some(settle(ctx.machine, result))
        }
    });
    let state = listing.read();
    loaded(
        state.as_ref().and_then(Option::as_ref),
        |(title, page): &(String, Page)| {
            rsx! { BrowseView { title: title.clone(), page: page.clone() } }
        },
    )
}

#[component]
fn TitleScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    let detail = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            Some(settle(
                ctx.machine,
                api.detail(&call("title", "load"), id).await,
            ))
        }
    });
    let state = detail.read();
    loaded(
        state.as_ref().and_then(Option::as_ref),
        |detail: &Detail| {
            rsx! { TitleView { detail: detail.clone() } }
        },
    )
}

/// A negotiated playback, ready for the player.
struct Playing {
    title: String,
    url: String,
    start_ms: u64,
    engine: Engine,
    /// The server's play session; a new one remounts the player.
    session: String,
    grant: NativeGrant,
}

/// The playback props, compared by identity: a new negotiation is a new
/// player.
#[derive(Clone)]
struct PlayingRef(Rc<Playing>);

impl PartialEq for PlayingRef {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

#[component]
fn WatchScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    // Where the player was, so a renegotiation after a reconnect carries on
    // from there rather than from the stored position.
    let last_ms = use_hook(|| Rc::new(Cell::new(0u64)));
    let resume_from = Rc::clone(&last_ms);
    let playing = use_resource(move || {
        let api = ctx.api.read().clone();
        let resume_from = Rc::clone(&resume_from);
        async move {
            let api = api?;
            let info = call("watch", "play");
            let result = async {
                let detail = api.detail(&info, id).await?;
                let (decision, grant) = api.play(&info, id, &platform::capabilities()).await?;
                Ok::<_, ClientError>((detail, decision, grant))
            }
            .await;
            Some(
                settle(ctx.machine, result).and_then(|(detail, decision, grant)| {
                    let engine = choose_engine(
                        decision.is_hls(),
                        platform::plays_hls(),
                        platform::has_mse(),
                    );
                    platform::info(&format!(
                        "pharos: playback method={} engine={engine:?} block={} downgrade={} url={}",
                        decision.method,
                        decision.direct_play_block,
                        decision.downgrade,
                        decision.stream_url
                    ));
                    if engine == Engine::Unsupported {
                        return Err(
                            "This title needs HLS, which this browser cannot play.".to_owned()
                        );
                    }
                    let start_ms = match resume_from.get() {
                        0 => start_ms(detail.item.position_ms, detail.item.played),
                        here => here,
                    };
                    Ok(PlayingRef(Rc::new(Playing {
                        title: detail.item.name,
                        url: decision.stream_url,
                        start_ms,
                        engine,
                        session: decision.play_session,
                        grant,
                    })))
                }),
            )
        }
    });
    let state = playing.read();
    loaded(state.as_ref().and_then(Option::as_ref), |p: &PlayingRef| {
        rsx! { Player { key: "{p.0.session}", playing: p.clone(), last_ms: LastMs(Rc::clone(&last_ms)) } }
    })
}

/// The player's last known position, shared with its screen.
#[derive(Clone)]
struct LastMs(Rc<Cell<u64>>);

impl PartialEq for LastMs {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// Attaches the engine and reports progress: on play, pause and seek, every
/// 10 s while playing, and one stop when the video ends, the player
/// unmounts or the page is hidden.
#[component]
fn Player(playing: PlayingRef, last_ms: LastMs) -> Element {
    let ctx = use_context::<Ctx>();
    let p = playing.0;
    let clock = use_hook(|| Rc::new(RefCell::new(ReportClock::new())));
    let engine = use_hook(|| Rc::new(RefCell::new(None::<Box<dyn PlaybackEngine>>)));
    let mut failed = use_signal(|| None::<String>);

    let stop = {
        let (p, clock, last_ms) = (Rc::clone(&p), Rc::clone(&clock), Rc::clone(&last_ms.0));
        move || {
            if !clock.borrow_mut().stop() {
                return;
            }
            let grant = p.grant.clone();
            let at = last_ms.get();
            platform::spawn_detached(async move {
                if let Err(e) = grant.stop(&call("watch", "stop"), at).await {
                    platform::warn(&format!("pharos: playback stop failed: {e}"));
                }
            });
        }
    };
    use_hook({
        let stop = stop.clone();
        move || Rc::new(platform::on_page_hide(stop))
    });
    use_drop({
        let (engine, stop) = (Rc::clone(&engine), stop.clone());
        move || {
            engine.borrow_mut().take();
            stop();
        }
    });
    // The engines call back from browser events, outside the Dioxus runtime.
    let fatal = use_coroutine({
        let stop = stop.clone();
        move |mut reasons: UnboundedReceiver<String>| {
            let stop = stop.clone();
            async move {
                while let Some(reason) = reasons.next().await {
                    stop();
                    failed.set(Some(format!("Playback failed: {reason}")));
                }
            }
        }
    });
    use_effect({
        let (p, engine) = (Rc::clone(&p), Rc::clone(&engine));
        move || {
            let (p, engine) = (Rc::clone(&p), Rc::clone(&engine));
            spawn(async move {
                let on_fatal: OnFatal = Rc::new(move |reason| fatal.send(reason));
                let attached = match platform::engine(p.engine).await {
                    Ok(mut e) => e.attach(VIDEO_ID, &p.url, p.start_ms, on_fatal).map(|()| e),
                    Err(e) => Err(e),
                };
                match attached {
                    Ok(e) => *engine.borrow_mut() = Some(e),
                    Err(e) => failed.set(Some(format!("Could not start playback: {e}"))),
                }
            });
        }
    });

    let on_media = {
        let (p, clock, last_ms) = (Rc::clone(&p), Rc::clone(&clock), Rc::clone(&last_ms.0));
        move |event: MediaEvent| {
            let at = platform::position_ms(VIDEO_ID);
            if at > 0 {
                last_ms.set(at);
            }
            let Some(send) = clock.borrow_mut().on(platform::now_ms(), event) else {
                return;
            };
            if send == Outgoing::Stop {
                stop();
                return;
            }
            let Outgoing::Report { paused } = send else {
                return;
            };
            let grant = p.grant.clone();
            let at = last_ms.get();
            spawn(async move {
                let result = grant.report(&call("watch", "report"), at, paused).await;
                if let Err(e) = settle(ctx.machine, result) {
                    platform::warn(&format!("pharos: progress report failed: {e}"));
                }
            });
        }
    };

    rsx! {
        if let Some(message) = failed() {
            ErrorNote { message }
        }
        WatchView { title: p.title.clone(), on_media }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_starts_by_connecting() {
        let mut dom = VirtualDom::new(Root);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Connecting…"), "{html}");
    }
}
