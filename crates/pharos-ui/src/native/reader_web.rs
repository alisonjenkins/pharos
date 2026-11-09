//! The book reader screen (spec 023): fetches an EPUB, hands it to `quire`
//! and draws it with `quire-web` in a host the stylesheet sizes. Rules
//! (settings, size cap, save timing, keys) are in `reader.rs`.
//!
//! Cost model, for TVs and headsets. Nothing animates beyond what
//! `quire-web` does for a page turn, no timer runs while reading (a save
//! is asked for on a turn, never on a clock), and leaving the screen drops
//! the view, which revokes the book's `blob:` URLs.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use gloo_net::http::Request;
use pharos_client::api::{BookFormat, ItemRef, NativeApi, PharosApi as _};
use quire::{Locator, Publication, Target};
use quire_web::{Action, Font, Settings, Start, Theme, View};

use super::app::{call, this_device, Ctx};
use super::keymap::Mode;
use super::platform;
use super::reader::{
    check_size, download_href, flatten_toc, key_action, margin_label, percent_label, save_key,
    saved_fraction, saved_locator, size_cap, too_big_message, KeyAction, PageTheme, ReaderSettings,
    SaveGate, SizeVerdict, TocRow, Typeface, LINE_HEIGHTS, MARGINS, SETTINGS_KEY,
};
use super::route::Route;
use super::views::{display_name, Nav};

const ROOT_ID: &str = "pharos-reader";
const HOST_ID: &str = "pharos-reader-host";

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Loading,
    Reading,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Panel {
    Contents,
    Text,
}

/// The pharos theme's own colours, through the custom properties that
/// cross the shadow boundary.
fn pharos_theme() -> Theme {
    Theme {
        text: "var(--px-color-fg)".into(),
        background: "var(--px-color-bg)".into(),
        link: "var(--px-color-accent)".into(),
    }
}

fn view_settings(s: ReaderSettings) -> Settings {
    Settings {
        font_size_px: f64::from(s.font_px),
        line_height: s.line_height(),
        font: match s.typeface {
            Typeface::Book => Font::Book,
            Typeface::Serif => Font::Serif,
            Typeface::Sans => Font::Sans,
        },
        margin_px: f64::from(s.margin_px),
        theme: match s.theme {
            PageTheme::Pharos => pharos_theme(),
            PageTheme::Paper => Theme::paper(),
            PageTheme::Sepia => Theme::sepia(),
            PageTheme::Night => Theme::night(),
        },
    }
}

fn stored_settings() -> ReaderSettings {
    match platform::stored_theme(SETTINGS_KEY) {
        Ok(Some(text)) => ReaderSettings::parse(&text),
        Ok(None) => ReaderSettings::default(),
        Err(e) => {
            platform::warn(&format!("pharos: reader settings read failed error={e}"));
            ReaderSettings::default()
        }
    }
}

/// Sends the position to the server when the gate allows.
struct Saver {
    api: NativeApi,
    id: ItemRef,
    gate: RefCell<SaveGate>,
}

impl Saver {
    fn send(&self, locator: &str, fraction: f32, forced: bool) {
        let key = save_key(locator, fraction);
        let now = platform::now_ms();
        if !self.gate.borrow().due(&key, now, forced) {
            return;
        }
        self.gate.borrow_mut().sent(&key, now);
        platform::info(&format!(
            "pharos: reader save item={} locator={locator} fraction={fraction:.3} forced={forced}",
            self.id
        ));
        let (api, id, locator) = (self.api.clone(), self.id, locator.to_owned());
        platform::spawn_detached(async move {
            if let Err(e) = api
                .set_read_position(&call("read", "save"), id, &locator, fraction)
                .await
            {
                platform::warn(&format!(
                    "pharos: reader save failed item={id} locator={locator} error={e}"
                ));
            }
        });
    }
}

/// An open book on screen. Holds the view; the view's callbacks hold only
/// a `Weak` of this, so dropping the screen frees both.
struct Core {
    view: View,
    saver: Saver,
    at_end: Cell<bool>,
    alive: Rc<Cell<bool>>,
    fraction: Signal<f64>,
}

impl Core {
    fn set_fraction(&self, f: f64) {
        if self.alive.get() {
            let mut shown = self.fraction;
            shown.set(f);
        }
    }

    /// The reader has gone to a new page: show how far, and offer a save.
    fn moved(&self, at_end: bool) {
        self.at_end.set(at_end);
        self.set_fraction(if at_end { 1.0 } else { self.view.progress() });
        self.save(false);
    }

    fn save(&self, forced: bool) {
        let locator = self.view.locator().to_string();
        let fraction = saved_fraction(self.view.progress(), self.at_end.get());
        self.saver.send(&locator, fraction, forced);
    }

    async fn turn(&self, forward: bool) {
        let more = if forward {
            self.view.next().await
        } else {
            self.view.prev().await
        };
        match more {
            Ok(more) => self.moved(forward && !more),
            Err(e) => platform::warn(&format!("pharos: reader turn failed error={e}")),
        }
    }

    async fn go(&self, target: &Target) {
        match self.view.go(target).await {
            Ok(()) => self.moved(false),
            Err(e) => platform::warn(&format!("pharos: reader jump failed error={e}")),
        }
    }
}

struct Opened {
    core: Rc<Core>,
    toc: Vec<TocRow>,
}

/// The book's bytes, refused before the body is read when a weak device
/// would hold too many of them.
async fn fetch_book(id: ItemRef) -> Result<Vec<u8>, String> {
    let cap = size_cap(this_device(), &platform::user_agent());
    let controller = web_sys::AbortController::new().map_err(|e| format!("{e:?}"))?;
    let response = Request::get(&download_href(id))
        .abort_signal(Some(&controller.signal()))
        .send()
        .await
        .map_err(|e| format!("The download failed: {e}"))?;
    if !response.ok() {
        return Err(format!(
            "The server answered {} {}.",
            response.status(),
            response.status_text()
        ));
    }
    let length = response
        .headers()
        .get("content-length")
        .and_then(|v| v.trim().parse::<u64>().ok());
    match check_size(length, cap) {
        SizeVerdict::Open => {}
        SizeVerdict::OpenUnknown => platform::warn(&format!(
            "pharos: reader size unknown item={id} (no Content-Length); opening"
        )),
        SizeVerdict::TooBig { bytes, cap } => {
            controller.abort();
            platform::warn(&format!(
                "pharos: reader refused item={id} bytes={bytes} cap={cap}"
            ));
            return Err(too_big_message(bytes, cap));
        }
    }
    response
        .binary()
        .await
        .map_err(|e| format!("The download failed: {e}"))
}

#[allow(clippy::too_many_arguments)]
async fn open_book(
    api: NativeApi,
    id: ItemRef,
    saved: Option<Locator>,
    settings: ReaderSettings,
    fraction: Signal<f64>,
    alive: Rc<Cell<bool>>,
    bytes: Vec<u8>,
    on_action: impl Fn(Action) + 'static,
) -> Result<Opened, String> {
    let book =
        Publication::open_epub(bytes).map_err(|e| format!("The book cannot be read: {e}"))?;
    let toc = flatten_toc(book.toc());
    let host = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(HOST_ID))
        .ok_or("The reader has no page to draw on.")?;
    let view = View::mount(&host, Rc::new(book), view_settings(settings))
        .map_err(|e| format!("The reader could not start: {e}"))?;
    view.on_action(on_action);
    let opened = match saved {
        Some(at) => view.go_to(at).await,
        None => view.open(0, Start::Beginning).await,
    };
    if let Err(e) = opened {
        view.unmount();
        return Err(format!("The book cannot be shown: {e}"));
    }
    let core = Rc::new(Core {
        view,
        saver: Saver {
            api,
            id,
            gate: RefCell::new(SaveGate::default()),
        },
        at_end: Cell::new(false),
        alive,
        fraction,
    });
    core.set_fraction(core.view.progress());
    Ok(Opened { core, toc })
}

fn leave(nav: Nav, id: ItemRef) {
    if !platform::go_back() {
        nav.0.call(Route::Title(id));
    }
}

fn flush(core: &RefCell<Option<Rc<Core>>>) {
    let held = core.borrow().clone();
    if let Some(c) = held {
        c.save(true);
    }
}

/// `/read/{id}`: the book, full screen.
#[component]
pub fn ReaderScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    let nav = use_context::<Nav>();
    super::keys::use_key_mode(Mode::Reader);
    let mut phase = use_signal(|| Phase::Loading);
    let mut title = use_signal(String::new);
    let mut toc = use_signal(Vec::<TocRow>::new);
    let fraction = use_signal(|| 0.0f64);
    let mut settings = use_signal(stored_settings);
    let mut panel = use_signal(|| None::<Panel>);
    let alive = use_hook(|| Rc::new(Cell::new(true)));
    let core = use_hook(|| Rc::new(RefCell::new(None::<Rc<Core>>)));
    let started = use_hook(|| Rc::new(Cell::new(false)));

    use_effect({
        let (alive, core, started) = (alive.clone(), core.clone(), started.clone());
        move || {
            let Some(api) = ctx.api.read().clone() else {
                return;
            };
            if started.replace(true) {
                return;
            }
            let (alive, core) = (alive.clone(), core.clone());
            spawn(async move {
                let began = platform::now_ms();
                let detail = match api.detail(&call("read", "load"), id).await {
                    Ok(d) => d,
                    Err(e) => {
                        platform::warn(&format!("pharos: reader failed item={id} error={e}"));
                        phase.set(Phase::Failed(format!("The book could not be loaded: {e}")));
                        return;
                    }
                };
                title.set(display_name(&detail.item));
                let format = detail.book.as_ref().map(|b| b.format);
                if format != Some(BookFormat::Epub) {
                    platform::warn(&format!(
                        "pharos: reader failed item={id} format={format:?} error=not an EPUB"
                    ));
                    phase.set(Phase::Failed("This book cannot be opened here.".into()));
                    return;
                }
                let bytes = match fetch_book(id).await {
                    Ok(b) => b,
                    Err(message) => {
                        platform::warn(&format!("pharos: reader failed item={id} error={message}"));
                        phase.set(Phase::Failed(message));
                        return;
                    }
                };
                let length = bytes.len();
                let saved = detail
                    .read_position
                    .as_ref()
                    .and_then(|p| saved_locator(&p.locator));
                // The view calls back from outside the runtime; it holds the
                // core weakly so the two cannot keep each other alive.
                let weak = Rc::new(RefCell::new(std::rc::Weak::<Core>::new()));
                let handler = {
                    let weak = weak.clone();
                    move |action: Action| {
                        let Some(c) = weak.borrow().upgrade() else {
                            return;
                        };
                        match action {
                            Action::Next => platform::spawn_detached(async move {
                                c.turn(true).await;
                            }),
                            Action::Prev => platform::spawn_detached(async move {
                                c.turn(false).await;
                            }),
                            Action::Jump(target) => platform::spawn_detached(async move {
                                c.go(&target).await;
                            }),
                            Action::Relayout => c.view.relayout(),
                        }
                    }
                };
                let opened = open_book(
                    api,
                    id,
                    saved,
                    *settings.peek(),
                    fraction,
                    alive.clone(),
                    bytes,
                    handler,
                )
                .await;
                match opened {
                    Ok(o) if alive.get() => {
                        let at = o.core.view.locator();
                        *weak.borrow_mut() = Rc::downgrade(&o.core);
                        toc.set(o.toc);
                        *core.borrow_mut() = Some(o.core);
                        phase.set(Phase::Reading);
                        platform::info(&format!(
                            "pharos: reader opened item={id} format=epub bytes={length} ms={:.0} resumed={} at={}",
                            platform::now_ms() - began,
                            saved.is_some(),
                            at
                        ));
                    }
                    Ok(o) => o.core.view.unmount(),
                    Err(message) => {
                        platform::warn(&format!(
                            "pharos: reader failed item={id} bytes={length} error={message}"
                        ));
                        phase.set(Phase::Failed(message));
                    }
                }
            });
        }
    });

    // Held for the screen's life; each removes its listener on drop.
    use_hook({
        let (alive, core) = (alive.clone(), core.clone());
        move || {
            let key_alive = alive.clone();
            let key_core = core.clone();
            let keys = platform::on_key(move |key, ctrl, meta| {
                if !key_alive.get() || platform::dialog_open() {
                    return false;
                }
                let Some(action) = key_action(key, ctrl, meta) else {
                    return false;
                };
                if action == KeyAction::Escape {
                    if panel.peek().is_some() {
                        let mut open = panel;
                        open.set(None);
                        platform::focus_by_id(ROOT_ID);
                    } else {
                        leave(nav, id);
                    }
                    return true;
                }
                if panel.peek().is_some() {
                    return false;
                }
                let held = key_core.borrow().clone();
                let Some(c) = held else {
                    return false;
                };
                platform::spawn_detached(async move {
                    c.turn(action == KeyAction::Next).await;
                });
                true
            });
            let (hide_core, hidden_core) = (core.clone(), core);
            let hide = platform::on_page_hide(move || flush(&hide_core));
            let lifecycle = platform::on_page_lifecycle(move || flush(&hidden_core), || {});
            Rc::new((keys, hide, lifecycle))
        }
    });

    use_drop({
        let (alive, core) = (alive.clone(), core.clone());
        move || {
            alive.set(false);
            let held = core.borrow_mut().take();
            if let Some(c) = held {
                c.save(true);
                c.view.unmount();
            }
        }
    });

    let apply = EventHandler::new({
        let core = core.clone();
        move |next: ReaderSettings| {
            settings.set(next);
            if let Err(e) = platform::store_theme(SETTINGS_KEY, &next.to_text()) {
                platform::warn(&format!("pharos: reader settings write failed error={e}"));
            }
            let held = core.borrow().clone();
            if let Some(c) = held {
                c.view.set_settings(view_settings(next));
            }
        }
    });
    let jump = EventHandler::new({
        let core = core.clone();
        move |target: Target| {
            panel.set(None);
            platform::focus_by_id(ROOT_ID);
            let held = core.borrow().clone();
            if let Some(c) = held {
                platform::spawn_detached(async move { c.go(&target).await });
            }
        }
    });

    let current = *settings.read();
    let page_background = view_settings(current).theme.background;
    let shown = phase.read().clone();
    let label = percent_label(*fraction.read());
    let width = format!("width: {}%", super::reader::percent(*fraction.read()));
    let open = *panel.read();
    let toggle = EventHandler::new(move |which: Panel| {
        let next = if *panel.peek() == Some(which) {
            None
        } else {
            Some(which)
        };
        panel.set(next);
        if next.is_none() {
            platform::focus_by_id(ROOT_ID);
        }
    });
    rsx! {
        div { class: "pharos-reader", id: ROOT_ID, tabindex: "-1",
            div { class: "pharos-reader-top",
                button {
                    class: "pharos-control",
                    r#type: "button",
                    aria_label: "Back",
                    onclick: move |_| leave(nav, id),
                    "←"
                }
                h2 { class: "pharos-reader-title", "{title}" }
                button {
                    class: "pharos-control",
                    r#type: "button",
                    aria_label: "Contents",
                    aria_pressed: "{open == Some(Panel::Contents)}",
                    onclick: move |_| toggle.call(Panel::Contents),
                    "Contents"
                }
                button {
                    class: "pharos-control",
                    r#type: "button",
                    aria_label: "Text settings",
                    aria_pressed: "{open == Some(Panel::Text)}",
                    onclick: move |_| toggle.call(Panel::Text),
                    "Aa"
                }
            }
            // The page theme fills the stage; the host, one measure wide at
            // the text size, sits in its middle.
            div { class: "pharos-reader-stage", style: "background: {page_background}",
                div {
                    class: "pharos-reader-host",
                    id: HOST_ID,
                    style: "font-size: {current.font_px}px",
                }
            }
            div {
                class: "pharos-reader-progress",
                role: "progressbar",
                aria_label: "Progress through the book",
                aria_valuemin: "0",
                aria_valuemax: "100",
                aria_valuenow: "{super::reader::percent(*fraction.read())}",
                div { class: "pharos-reader-progress-bar", style: "{width}" }
                span { class: "pharos-reader-progress-label", "{label}" }
            }
            match shown {
                Phase::Loading => rsx! {
                    p { class: "pharos-reader-note", role: "status", "Opening the book…" }
                },
                Phase::Failed(message) => rsx! {
                    div { class: "pharos-reader-note", role: "alert",
                        p { "{message}" }
                        a {
                            class: "pharos-button",
                            href: download_href(id),
                            download: "",
                            "Download"
                        }
                    }
                },
                Phase::Reading => rsx! {},
            }
            if open == Some(Panel::Contents) {
                nav { class: "pharos-reader-panel", aria_label: "Table of contents",
                    ul {
                        for (i, row) in toc.read().iter().cloned().enumerate() {
                            li { key: "{i}", style: "padding-left: {row.depth}rem",
                                match row.target.clone() {
                                    Some(target) => rsx! {
                                        button {
                                            class: "pharos-reader-toc-row",
                                            r#type: "button",
                                            onclick: move |_| jump.call(target.clone()),
                                            "{row.title}"
                                        }
                                    },
                                    None => rsx! { span { class: "pharos-muted", "{row.title}" } },
                                }
                            }
                        }
                    }
                }
            }
            if open == Some(Panel::Text) {
                div { class: "pharos-reader-panel", role: "region", aria_label: "Text settings",
                    div { class: "pharos-reader-setting",
                        span { "Text size" }
                        button {
                            class: "pharos-control",
                            r#type: "button",
                            aria_label: "Smaller text",
                            disabled: !current.can_step_font(-1),
                            onclick: move |_| apply.call(current.font_step(-1)),
                            "A−"
                        }
                        span { class: "pharos-reader-size", aria_label: "Text size in pixels", "{current.font_px}" }
                        button {
                            class: "pharos-control",
                            r#type: "button",
                            aria_label: "Larger text",
                            disabled: !current.can_step_font(1),
                            onclick: move |_| apply.call(current.font_step(1)),
                            "A+"
                        }
                    }
                    div { class: "pharos-reader-setting", role: "group", aria_label: "Typeface",
                        span { "Typeface" }
                        for face in Typeface::ALL {
                            button {
                                class: "pharos-control",
                                r#type: "button",
                                aria_pressed: "{current.typeface == face}",
                                onclick: move |_| apply.call(ReaderSettings { typeface: face, ..current }),
                                "{face.label()}"
                            }
                        }
                    }
                    div { class: "pharos-reader-setting", role: "group", aria_label: "Line spacing",
                        span { "Line spacing" }
                        for h in LINE_HEIGHTS {
                            button {
                                class: "pharos-control",
                                r#type: "button",
                                aria_pressed: "{current.line_height_x10 == h}",
                                onclick: move |_| apply.call(ReaderSettings { line_height_x10: h, ..current }),
                                "{h / 10}.{h % 10}"
                            }
                        }
                    }
                    div { class: "pharos-reader-setting", role: "group", aria_label: "Margins",
                        span { "Margins" }
                        for m in MARGINS {
                            button {
                                class: "pharos-control",
                                r#type: "button",
                                aria_pressed: "{current.margin_px == m}",
                                onclick: move |_| apply.call(ReaderSettings { margin_px: m, ..current }),
                                "{margin_label(m)}"
                            }
                        }
                    }
                    div { class: "pharos-reader-setting", role: "group", aria_label: "Page theme",
                        span { "Page" }
                        for theme in PageTheme::ALL {
                            button {
                                class: "pharos-control",
                                r#type: "button",
                                aria_pressed: "{current.theme == theme}",
                                onclick: move |_| apply.call(ReaderSettings { theme, ..current }),
                                "{theme.label()}"
                            }
                        }
                    }
                }
            }
        }
    }
}
