//! The native app: routing and history, the connection lifecycle that
//! drives [`Machine`], and the screens that load data for the pure views.

use std::rc::Rc;

use dioxus::prelude::*;
use pharos_client::api::{
    Decision, Detail, Home, ItemRef, NativeApi, NativeGrant, Page, PharosApi as _,
};
use pharos_client::{CallInfo, ClientError};

use super::machine::{Event, Machine, Phase};
use super::platform::{self, OpenError};
use super::route::Route;
use super::views::{
    BrowseView, ErrorNote, HomeView, Loading, Nav, Shell, SignInView, StatusView, TitleView,
    WatchView,
};
use crate::views::RootApp;

/// Items a browse page asks for at once.
const BROWSE_PAGE: u32 = 100;

/// The page root: one route per URL, kept in step with history. `/manage`
/// mounts the older REST panes and never opens the native socket.
#[component]
pub fn Root() -> Element {
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
                api.set(None);
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

    let phase = machine.read().phase().clone();
    match phase {
        Phase::Connecting => rsx! {
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
        Phase::Ready => {
            let notice = machine.read().notice().map(str::to_owned);
            rsx! {
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
            }
        }
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

/// A negotiated playback. The grant lives as long as the screen, so the
/// server keeps the session it started.
struct Playing {
    title: String,
    decision: Decision,
    _grant: NativeGrant,
}

#[component]
fn WatchScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    let playing = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            let info = call("watch", "play");
            let result = async {
                let detail = api.detail(&info, id).await?;
                let (decision, grant) = api.play(&info, id, &platform::capabilities()).await?;
                Ok::<_, ClientError>(Playing {
                    title: detail.item.name,
                    decision,
                    _grant: grant,
                })
            }
            .await;
            Some(settle(ctx.machine, result).and_then(|p| {
                if p.decision.is_hls() && !platform::plays_hls() {
                    Err("This title needs HLS, which this browser cannot play yet.".to_owned())
                } else {
                    Ok(p)
                }
            }))
        }
    });
    let state = playing.read();
    loaded(state.as_ref().and_then(Option::as_ref), |p: &Playing| {
        rsx! { WatchView { title: p.title.clone(), src: p.decision.stream_url.clone() } }
    })
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
