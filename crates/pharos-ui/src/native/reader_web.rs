//! The book reader screen (spec 023): fetches an EPUB, hands it to `quire`
//! and draws it with `quire-web` in a host the stylesheet sizes; a PDF is
//! drawn page by page on a canvas by `pdf_web.rs`. Rules (settings, size
//! cap, save timing, keys) are in `reader.rs`.
//!
//! Cost model, for TVs and headsets. Nothing animates beyond what
//! `quire-web` does for a page turn, no timer runs while reading (a save
//! is asked for on a turn, never on a clock), and leaving the screen drops
//! the view, which revokes the book's `blob:` URLs.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use gloo_net::http::Request;
use pharos_client::api::{BookFormat, ItemRef, NativeApi, PharosApi as _, ReadPosition};
use quire::{Locator, Publication, Target};
use quire_web::{Action, Font, Settings, Start, Theme, View};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast as _;

use super::app::{call, this_device, Ctx};
use super::kept::{resume_place, server_place_to_adopt};
use super::kept_web;
use super::keymap::Mode;
use super::pdf_web::PdfView;
use super::platform;
use super::reader::{
    check_size, download_href, flatten_toc, key_action, margin_label, pdf_key, pdf_locator,
    pdf_progress, pdf_saved_index, progress_label, retry_delay_ms, retryable, save_key,
    saved_fraction, saved_locator, size_cap, swipe_action, tap_action, too_big_message, KeyAction,
    KeyPress, KeyTarget, PageTheme, PdfFit, PdfKey, PendingSave, PendingSaves, ReaderSettings,
    SaveGate, SizeVerdict, TocRow, Typeface, LINE_HEIGHTS, MARGINS, OFFLINE_NOTE, PENDING_KEY,
    SETTINGS_KEY,
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

/// The saves still waiting for the server, read from this device.
fn load_pending() -> PendingSaves {
    match platform::stored_theme(PENDING_KEY) {
        Ok(Some(text)) => {
            let (queue, dropped) = PendingSaves::parse_counting(&text);
            if dropped > 0 {
                platform::warn(&format!(
                    "pharos: reader pending dropped {dropped} entries with no item id"
                ));
                store_pending(&queue);
            }
            queue
        }
        Ok(None) => PendingSaves::default(),
        Err(e) => {
            platform::warn(&format!("pharos: reader pending read failed error={e}"));
            PendingSaves::default()
        }
    }
}

fn store_pending(queue: &PendingSaves) {
    if let Err(e) = platform::store_theme(PENDING_KEY, &queue.to_text()) {
        platform::warn(&format!(
            "pharos: reader pending write failed queue={} error={e}",
            queue.len()
        ));
    }
}

/// When the reader read the page now in front of them, on this clock.
fn read_now() -> i64 {
    // Whole milliseconds are all `readAt` carries.
    #[allow(clippy::cast_possible_truncation)]
    let ms = platform::wall_clock_ms() as i64;
    ms
}

/// What the server did with a save.
enum Delivery {
    /// It answered: the position was stored, or a newer one was kept.
    Answered,
    /// The connection or the server failed; send it again later.
    Retry,
    /// The server refused it; sending it again would be refused too.
    Refused,
}

/// Sends saves, keeps those the server has not answered on this device
/// (spec 036 FR-001) and sends them again, wherever the user is in the app:
/// when the app starts, when the API comes back, when the browser goes
/// online, when the reader opens and, while any is queued, with a growing
/// wait. Nothing runs while the queue is empty. One lives for the app, in
/// context.
#[derive(Clone)]
pub(super) struct OutboxCtx(Rc<Outbox>);

struct Outbox {
    api: Signal<Option<NativeApi>>,
    /// Bumped whenever the queue changes, so what shows it redraws.
    changed: Signal<u32>,
    /// Counts [`Outbox::kick`]s so an older retry loop stops when a newer one starts.
    generation: Cell<u32>,
}

impl Outbox {
    async fn deliver(&self, id: ItemRef, save: &PendingSave) -> Delivery {
        let Some(api) = self.api.peek().clone() else {
            return Delivery::Retry;
        };
        match api
            .set_read_position(
                &call("read", "save"),
                id,
                &save.locator,
                save.fraction,
                save.read_at_ms,
            )
            .await
        {
            Ok(_) => Delivery::Answered,
            Err(e) if retryable(&e) => {
                platform::warn(&format!(
                    "pharos: reader save failed item={id} locator={} error={e}",
                    save.locator
                ));
                Delivery::Retry
            }
            Err(e) => {
                platform::warn(&format!(
                    "pharos: reader save refused item={id} locator={} error={e}",
                    save.locator
                ));
                Delivery::Refused
            }
        }
    }

    fn refresh_note(&self) {
        let mut changed = self.changed;
        let next = changed.peek().wrapping_add(1);
        changed.set(next);
    }

    /// Sends `save` now; if the server cannot be reached it is queued.
    fn submit(self: &Rc<Self>, id: ItemRef, save: PendingSave) {
        let this = self.clone();
        platform::spawn_detached(async move {
            match this.deliver(id, &save).await {
                Delivery::Answered => {
                    if !load_pending().is_empty() {
                        this.kick();
                    }
                }
                Delivery::Retry => {
                    let mut queue = load_pending();
                    queue.queue(id, save.clone());
                    store_pending(&queue);
                    platform::info(&format!(
                        "pharos: reader save queued item={id} locator={} read_at={} queue={}",
                        save.locator,
                        save.read_at_ms,
                        queue.len()
                    ));
                    this.refresh_note();
                    this.kick();
                }
                Delivery::Refused => {}
            }
        });
    }

    /// Starts sending what is queued, now, and again with a growing wait
    /// until it is empty or the reader leaves. A newer kick replaces it.
    fn kick(self: &Rc<Self>) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let this = self.clone();
        platform::spawn_detached(async move {
            let mut attempt = 0;
            loop {
                if this.generation.get() != generation {
                    return;
                }
                let queue = load_pending();
                if queue.is_empty() {
                    this.refresh_note();
                    return;
                }
                for (id, save) in queue.entries() {
                    match this.deliver(id, &save).await {
                        Delivery::Retry => break,
                        Delivery::Answered | Delivery::Refused => {
                            let mut now = load_pending();
                            now.answered(id, &save);
                            store_pending(&now);
                            platform::info(&format!(
                                "pharos: reader save flushed item={id} locator={} queue={}",
                                save.locator,
                                now.len()
                            ));
                        }
                    }
                }
                this.refresh_note();
                let left = load_pending();
                if left.is_empty() {
                    return;
                }
                let wait = retry_delay_ms(attempt);
                platform::warn(&format!(
                    "pharos: reader flush waiting queue={} attempt={attempt} retry_ms={wait}",
                    left.len()
                ));
                attempt += 1;
                platform::sleep_ms(wait).await;
            }
        });
    }
}

/// A window `online` listener; removed on drop.
struct OnlineWatch(Closure<dyn FnMut(web_sys::Event)>);

impl OnlineWatch {
    fn new(mut on_online: impl FnMut() + 'static) -> Self {
        let listener = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| on_online());
        if let Some(window) = web_sys::window() {
            let _ = window
                .add_event_listener_with_callback("online", listener.as_ref().unchecked_ref());
        }
        Self(listener)
    }
}

impl Drop for OnlineWatch {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window
                .remove_event_listener_with_callback("online", self.0.as_ref().unchecked_ref());
        }
    }
}

/// Called once by the app: puts the outbox in context and flushes the queue
/// when the app starts (once the API is up), when the API is replaced by a
/// reconnect and when the browser goes online.
pub(super) fn use_pending_flush(api: Signal<Option<NativeApi>>) {
    let changed = use_signal(|| 0u32);
    let outbox = use_context_provider(|| {
        OutboxCtx(Rc::new(Outbox {
            api,
            changed,
            generation: Cell::new(0),
        }))
    })
    .0;
    use_hook({
        let outbox = outbox.clone();
        move || Rc::new(OnlineWatch::new(move || outbox.kick()))
    });
    use_effect(move || {
        if api.read().is_some() && !load_pending().is_empty() {
            outbox.kick();
        }
    });
}

/// Offers the position to the server when the gate allows.
struct Saver {
    outbox: Rc<Outbox>,
    id: ItemRef,
    /// The book is kept on this device: each place is also noted in its
    /// record, for a resume with no server.
    kept: bool,
    gate: RefCell<SaveGate>,
}

impl Saver {
    /// `read_at_ms` is when the reader reached this place, not when it is sent.
    fn send(&self, locator: &str, fraction: f32, read_at_ms: i64, forced: bool) {
        let key = save_key(locator, fraction);
        let now = platform::now_ms();
        // While a save is queued the server is out of reach and a save costs
        // one local write, so the newest place is kept at every turn.
        let forced = forced || load_pending().contains(self.id);
        if !self.gate.borrow().due(&key, now, forced) {
            return;
        }
        self.gate.borrow_mut().sent(&key, now);
        platform::info(&format!(
            "pharos: reader save item={} locator={locator} fraction={fraction:.3} forced={forced}",
            self.id
        ));
        if self.kept {
            kept_web::note_position(self.id, locator, fraction, read_at_ms);
        }
        self.outbox.submit(
            self.id,
            PendingSave {
                locator: locator.to_owned(),
                fraction,
                read_at_ms,
            },
        );
    }
}

/// What draws the open book: quire-web's text view or a PDF canvas.
enum Engine {
    Epub(View),
    Pdf(Rc<PdfView>),
}

impl Engine {
    fn locator(&self) -> String {
        match self {
            Engine::Epub(view) => view.locator().to_string(),
            Engine::Pdf(pdf) => pdf_locator(pdf.index()),
        }
    }

    fn progress(&self) -> f64 {
        match self {
            Engine::Epub(view) => view.progress(),
            Engine::Pdf(pdf) => pdf_progress(pdf.index(), pdf.count()),
        }
    }

    /// The PDF page index (from 0) and page count; `None` for an EPUB.
    fn page(&self) -> Option<(usize, usize)> {
        match self {
            Engine::Epub(_) => None,
            Engine::Pdf(pdf) => Some((pdf.index(), pdf.count())),
        }
    }

    fn unmount(&self) {
        match self {
            Engine::Epub(view) => view.unmount(),
            Engine::Pdf(pdf) => pdf.unmount(),
        }
    }
}

/// An open book on screen. Holds the engine; the view's callbacks hold only
/// a `Weak` of this, so dropping the screen frees both.
struct Core {
    engine: Engine,
    saver: Saver,
    /// When the reader reached the page now shown (unix ms).
    read_at: Cell<i64>,
    /// The reader has turned a page since the book opened.
    turned: Cell<bool>,
    at_end: Cell<bool>,
    alive: Rc<Cell<bool>>,
    fraction: Signal<f64>,
    page: Signal<Option<(usize, usize)>>,
}

impl Core {
    fn set_fraction(&self, f: f64) {
        if self.alive.get() {
            let mut shown = self.fraction;
            shown.set(f);
            let mut page = self.page;
            page.set(self.engine.page());
        }
    }

    /// The reader has gone to a new page: show how far, and offer a save.
    fn moved(&self, at_end: bool) {
        self.at_end.set(at_end);
        self.turned.set(true);
        self.read_at.set(read_now());
        self.set_fraction(if at_end { 1.0 } else { self.engine.progress() });
        self.save(false);
    }

    fn save(&self, forced: bool) {
        let locator = self.engine.locator();
        let fraction = saved_fraction(self.engine.progress(), self.at_end.get());
        self.saver
            .send(&locator, fraction, self.read_at.get(), forced);
    }

    async fn turn(&self, forward: bool) {
        let more = match &self.engine {
            Engine::Epub(view) => {
                let more = if forward {
                    view.next().await
                } else {
                    view.prev().await
                };
                more.map_err(|e| e.to_string())
            }
            Engine::Pdf(pdf) => pdf.turn(forward),
        };
        match more {
            Ok(more) => self.moved(forward && !more),
            Err(e) => platform::warn(&format!("pharos: reader turn failed error={e}")),
        }
    }

    async fn go(&self, target: &Target) {
        let Engine::Epub(view) = &self.engine else {
            return;
        };
        match view.go(target).await {
            Ok(()) => self.moved(false),
            Err(e) => platform::warn(&format!("pharos: reader jump failed error={e}")),
        }
    }

    /// Moves a kept book to the place another device read since this one
    /// noted its own, and records it as read when that device read it. A
    /// reader who turned a page meanwhile is left where they are.
    async fn adopt(&self, place: &ReadPosition) -> Result<(), String> {
        match &self.engine {
            Engine::Epub(view) => {
                let at = saved_locator(&place.locator)
                    .ok_or_else(|| format!("{} is not an EPUB place", place.locator))?;
                view.go_to(at).await.map_err(|e| e.to_string())?;
            }
            Engine::Pdf(pdf) => {
                pdf.go_to(pdf_saved_index(&place.locator, usize::MAX))?;
            }
        }
        if self.turned.get() {
            return Ok(());
        }
        self.read_at.set(place.read_at_ms);
        self.at_end.set(false);
        self.set_fraction(self.engine.progress());
        kept_web::note_position(
            self.saver.id,
            &place.locator,
            place.fraction,
            place.read_at_ms,
        );
        Ok(())
    }

    fn apply_settings(&self, settings: ReaderSettings) {
        match &self.engine {
            Engine::Epub(view) => view.set_settings(view_settings(settings)),
            Engine::Pdf(pdf) => pdf.set_fit(settings.pdf_fit),
        }
    }
}

/// What the reader needs of a book before its bytes: from the server's
/// detail, or from the record of a kept book.
struct Head {
    title: String,
    format: Option<BookFormat>,
    saved: Option<String>,
}

/// The book's bytes: a kept copy first, even online (spec 036 FR-006), then
/// the network. Says where they came from for the log.
async fn book_bytes(
    id: ItemRef,
    kept_user: Option<&str>,
    online: bool,
) -> Result<(js_sys::Uint8Array, &'static str), String> {
    if let Some(user) = kept_user {
        match kept_web::cached_book(user, id).await {
            Some(bytes) => {
                platform::info(&format!(
                    "pharos: reader cache hit item={id} bytes={}",
                    bytes.length()
                ));
                return Ok((bytes, "cache"));
            }
            None => platform::warn(&format!(
                "pharos: reader cache miss item={id}; the kept copy is gone"
            )),
        }
    }
    if !online {
        return Err("This book is not on this device and there is no connection.".into());
    }
    fetch_book(id).await.map(|bytes| (bytes, "network"))
}

/// Once a kept book is open, asks the server who is signed in, for at most
/// [`kept_web::SERVER_CHECK_MS`], and closes the book if the copy was kept
/// by someone else (FR-005). Never delays the book: a dead connection that
/// still reports online (a tunnel) just leaves it open.
/// Once a kept book is open, asks the server for its place, for at most
/// [`kept_web::SERVER_CHECK_MS`], and moves to it when another device read
/// further since this one did (spec 036 US4). Never delays the book.
fn check_place_later(
    id: ItemRef,
    api: Signal<Option<NativeApi>>,
    alive: Rc<Cell<bool>>,
    core: Rc<RefCell<Option<Rc<Core>>>>,
) {
    platform::spawn_detached(async move {
        let Some(api) = api.peek().clone() else {
            return;
        };
        if kept_web::is_offline() {
            return;
        }
        let asked = kept_web::within(
            kept_web::SERVER_CHECK_MS,
            api.detail(&call("read", "place"), id),
        )
        .await;
        let detail = match asked {
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                platform::warn(&format!(
                    "pharos: reader could not fetch the server place item={id} error={e}"
                ));
                return;
            }
            None => {
                platform::warn(&format!(
                    "pharos: reader could not fetch the server place item={id} error=no answer in {} ms",
                    kept_web::SERVER_CHECK_MS
                ));
                return;
            }
        };
        let held = core.borrow().clone();
        let (true, Some(c), Some(store)) = (alive.get(), held, kept_web::read()) else {
            return;
        };
        let Some(book) = store.find(&id.to_string()) else {
            return;
        };
        let queued = load_pending()
            .entries()
            .into_iter()
            .find(|(queued, _)| *queued == id)
            .map(|(_, save)| save);
        let Some(place) = server_place_to_adopt(
            book,
            queued.as_ref(),
            detail.read_position.as_ref(),
            c.turned.get(),
        ) else {
            return;
        };
        match c.adopt(place).await {
            Ok(()) => platform::info(&format!(
                "pharos: reader moved to the server place item={id} at={} read_at={}",
                place.locator, place.read_at_ms
            )),
            Err(e) => platform::warn(&format!(
                "pharos: reader could not move to the server place item={id} error={e}"
            )),
        }
    });
}

fn check_user_later(
    user: String,
    id: ItemRef,
    api: Signal<Option<NativeApi>>,
    alive: Rc<Cell<bool>>,
    core: Rc<RefCell<Option<Rc<Core>>>>,
    mut phase: Signal<Phase>,
) {
    platform::spawn_detached(async move {
        // The connection may only come up after the book is open.
        let Some(api) = api.peek().clone() else {
            return;
        };
        if kept_web::is_offline() {
            return;
        }
        let asked = kept_web::within(kept_web::SERVER_CHECK_MS, api.me(&call("read", "me"))).await;
        let me = match asked {
            Some(Ok(me)) => me,
            Some(Err(e)) => {
                platform::warn(&format!(
                    "pharos: reader could not confirm the user item={id} error={e}"
                ));
                return;
            }
            None => {
                platform::warn(&format!(
                    "pharos: reader could not confirm the user item={id} error=no answer in {} ms",
                    kept_web::SERVER_CHECK_MS
                ));
                return;
            }
        };
        if me.id == user || !alive.get() || !kept_web::reconcile(&me.id) {
            return;
        }
        platform::warn(&format!(
            "pharos: reader closes kept copy item={id} reason=another user"
        ));
        let held = core.borrow_mut().take();
        if let Some(c) = held {
            c.engine.unmount();
        }
        phase.set(Phase::Failed(
            "This book was kept by another account and has been removed from this device.".into(),
        ));
    });
}

struct Opened {
    core: Rc<Core>,
    toc: Vec<TocRow>,
}

/// Aborts the request when dropped, so leaving the screen mid-download (the
/// future is dropped with it) stops the transfer.
struct AbortOnDrop(web_sys::AbortController);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The book's bytes as the browser holds them, refused before the body is
/// read when a weak device would hold too many of them. A PDF goes to its
/// module from here without a copy through this module's memory.
async fn fetch_book(id: ItemRef) -> Result<js_sys::Uint8Array, String> {
    let cap = size_cap(this_device(), &platform::user_agent());
    let controller = AbortOnDrop(web_sys::AbortController::new().map_err(|e| format!("{e:?}"))?);
    let response = Request::get(&download_href(id))
        .abort_signal(Some(&controller.0.signal()))
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
            platform::warn(&format!(
                "pharos: reader refused item={id} bytes={bytes} cap={cap}"
            ));
            return Err(too_big_message(bytes, cap));
        }
    }
    let failed = |e: wasm_bindgen::JsValue| format!("The download failed: {e:?}");
    let raw: web_sys::Response = response.into();
    let promise = raw.array_buffer().map_err(failed)?;
    let buffer = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(failed)?;
    Ok(js_sys::Uint8Array::new(&buffer))
}

#[allow(clippy::too_many_arguments)]
async fn open_book(
    outbox: Rc<Outbox>,
    kept: bool,
    id: ItemRef,
    saved: Option<Locator>,
    settings: ReaderSettings,
    fraction: Signal<f64>,
    page: Signal<Option<(usize, usize)>>,
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
        Some(at) => match view.go_to(at).await {
            Ok(()) => Ok(()),
            // A book replaced by one with fewer sections leaves a place that
            // is not in it; open at the start rather than never.
            Err(e) => {
                platform::warn(&format!(
                    "pharos: reader saved place unusable item={id} locator={at} error={e}; opening at the start"
                ));
                view.open(0, Start::Beginning).await
            }
        },
        None => view.open(0, Start::Beginning).await,
    };
    if let Err(e) = opened {
        view.unmount();
        return Err(format!("The book cannot be shown: {e}"));
    }
    let core = Rc::new(Core {
        engine: Engine::Epub(view),
        saver: Saver {
            outbox,
            id,
            kept,
            gate: RefCell::new(SaveGate::default()),
        },
        read_at: Cell::new(read_now()),
        turned: Cell::new(false),
        at_end: Cell::new(false),
        alive,
        fraction,
        page,
    });
    core.set_fraction(core.engine.progress());
    Ok(Opened { core, toc })
}

/// Opens a PDF on the page `saved` names, if any.
#[allow(clippy::too_many_arguments)]
async fn open_pdf(
    outbox: Rc<Outbox>,
    kept: bool,
    id: ItemRef,
    saved: Option<&str>,
    settings: ReaderSettings,
    fraction: Signal<f64>,
    page: Signal<Option<(usize, usize)>>,
    alive: Rc<Cell<bool>>,
    bytes: &js_sys::Uint8Array,
    on_broken: Box<dyn Fn(String)>,
) -> Result<Opened, String> {
    let host = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(HOST_ID))
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
        .ok_or("The reader has no page to draw on.")?;
    // The count is only known once the file is open, so a saved page is held
    // inside the book by the view.
    let wanted = saved.map_or(0, |text| pdf_saved_index(text, usize::MAX));
    let view = PdfView::open(host, bytes, wanted, settings.pdf_fit, id, on_broken).await?;
    let core = Rc::new(Core {
        engine: Engine::Pdf(view),
        saver: Saver {
            outbox,
            id,
            kept,
            gate: RefCell::new(SaveGate::default()),
        },
        read_at: Cell::new(read_now()),
        turned: Cell::new(false),
        at_end: Cell::new(false),
        alive,
        fraction,
        page,
    });
    core.set_fraction(core.engine.progress());
    Ok(Opened {
        core,
        toc: Vec::new(),
    })
}

/// A page-wide `keydown` listener that reports the press and what it came
/// from; a taken press is not the browser's too. Removed on drop.
struct KeyWatch(Closure<dyn FnMut(web_sys::KeyboardEvent)>);

impl KeyWatch {
    fn new(mut on_press: impl FnMut(KeyPress<'_>) -> bool + 'static) -> Self {
        let listener =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
                let target = e
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    .map_or(KeyTarget::Page, |el| {
                        if el
                            .closest("input, select, textarea")
                            .ok()
                            .flatten()
                            .is_some()
                        {
                            KeyTarget::Field
                        } else if el.closest("button, a").ok().flatten().is_some() {
                            KeyTarget::Control
                        } else {
                            KeyTarget::Page
                        }
                    });
                let key = e.key();
                let press = KeyPress {
                    key: &key,
                    ctrl: e.ctrl_key(),
                    meta: e.meta_key(),
                    alt: e.alt_key(),
                    shift: e.shift_key(),
                    target,
                };
                if on_press(press) {
                    e.prevent_default();
                }
            });
        if let Some(window) = web_sys::window() {
            let _ = window
                .add_event_listener_with_callback("keydown", listener.as_ref().unchecked_ref());
        }
        Self(listener)
    }
}

impl Drop for KeyWatch {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window
                .remove_event_listener_with_callback("keydown", self.0.as_ref().unchecked_ref());
        }
    }
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
    let page = use_signal(|| None::<(usize, usize)>);
    let mut is_pdf = use_signal(|| false);
    let swipe_from = use_hook(|| Rc::new(Cell::new(f64::NAN)));
    let swiped = use_hook(|| Rc::new(Cell::new(false)));
    let mut settings = use_signal(stored_settings);
    let mut panel = use_signal(|| None::<Panel>);
    let alive = use_hook(|| Rc::new(Cell::new(true)));
    let core = use_hook(|| Rc::new(RefCell::new(None::<Rc<Core>>)));
    let started = use_hook(|| Rc::new(Cell::new(false)));
    let outbox = use_context::<OutboxCtx>().0;

    // Opening the book is a chance to send what was queued.
    use_effect({
        let outbox = outbox.clone();
        move || {
            if ctx.api.read().is_some() && !load_pending().is_empty() {
                outbox.kick();
            }
        }
    });

    use_effect({
        let (alive, core, started, outbox) =
            (alive.clone(), core.clone(), started.clone(), outbox.clone());
        move || {
            let api = ctx.api.read().clone();
            let held = kept_web::read().and_then(|store| {
                let book = store.find(&id.to_string())?.clone();
                Some((store.user.clone(), book))
            });
            // A kept book opens from the device without waiting for the
            // server; any other needs the connection.
            if api.is_none() && held.is_none() {
                return;
            }
            if started.replace(true) {
                return;
            }
            let (alive, core, outbox) = (alive.clone(), core.clone(), outbox.clone());
            let live_api = ctx.api;
            spawn(async move {
                let began = platform::now_ms();
                let head = match (&held, &api) {
                    (Some((_, book)), _) => {
                        let queued = load_pending()
                            .entries()
                            .into_iter()
                            .find(|(queued, _)| *queued == id)
                            .map(|(_, save)| save);
                        Head {
                            title: book.title.clone(),
                            format: Some(book.format.book_format()),
                            saved: resume_place(book, queued.as_ref()).map(|(locator, _)| locator),
                        }
                    }
                    (None, Some(api)) => match api.detail(&call("read", "load"), id).await {
                        Ok(d) => Head {
                            title: display_name(&d.item),
                            format: d.book.as_ref().map(|b| b.format),
                            saved: d.read_position.as_ref().map(|p| p.locator.clone()),
                        },
                        Err(e) => {
                            platform::warn(&format!("pharos: reader failed item={id} error={e}"));
                            phase.set(Phase::Failed(format!("The book could not be loaded: {e}")));
                            return;
                        }
                    },
                    (None, None) => return,
                };
                title.set(head.title.clone());
                let format = head.format;
                if !matches!(format, Some(BookFormat::Epub | BookFormat::Pdf)) {
                    platform::warn(&format!(
                        "pharos: reader failed item={id} format={format:?} error=not an EPUB or PDF"
                    ));
                    phase.set(Phase::Failed("This book cannot be opened here.".into()));
                    return;
                }
                let kept_user = held.as_ref().map(|(user, _)| user.clone());
                let kept = kept_user.is_some();
                let online = api.is_some() && !kept_web::is_offline();
                if let Some(user) = kept_user.clone() {
                    check_user_later(user, id, live_api, alive.clone(), core.clone(), phase);
                }
                if format == Some(BookFormat::Pdf) {
                    is_pdf.set(true);
                    let (bytes, from) = match book_bytes(id, kept_user.as_deref(), online).await {
                        Ok(b) => b,
                        Err(message) => {
                            platform::warn(&format!(
                                "pharos: reader failed item={id} error={message}"
                            ));
                            phase.set(Phase::Failed(message));
                            return;
                        }
                    };
                    let length = bytes.length();
                    let saved = head.saved.clone();
                    let opened = open_pdf(
                        outbox,
                        kept,
                        id,
                        saved.as_deref(),
                        *settings.peek(),
                        fraction,
                        page,
                        alive.clone(),
                        &bytes,
                        // The module trapped: say so, with the download, and
                        // let the next open start a fresh module.
                        Box::new({
                            let alive = alive.clone();
                            move |message| {
                                if alive.get() {
                                    platform::warn(&format!(
                                        "pharos: reader failed item={id} error={message}"
                                    ));
                                    let mut phase = phase;
                                    phase.set(Phase::Failed(message));
                                }
                            }
                        }),
                    )
                    .await;
                    match opened {
                        Ok(o) if alive.get() => {
                            let at = o.core.engine.locator();
                            *core.borrow_mut() = Some(o.core);
                            phase.set(Phase::Reading);
                            if kept {
                                check_place_later(id, live_api, alive.clone(), core.clone());
                            }
                            platform::info(&format!(
                                "pharos: reader opened item={id} format=pdf from={from} bytes={length} ms={:.0} resumed={} at={}",
                                platform::now_ms() - began,
                                saved.is_some(),
                                at
                            ));
                        }
                        Ok(o) => o.core.engine.unmount(),
                        Err(message) => {
                            platform::warn(&format!(
                                "pharos: reader failed item={id} bytes={length} error={message}"
                            ));
                            phase.set(Phase::Failed(message));
                        }
                    }
                    return;
                }
                let (bytes, from) = match book_bytes(id, kept_user.as_deref(), online).await {
                    Ok(b) => b,
                    Err(message) => {
                        platform::warn(&format!("pharos: reader failed item={id} error={message}"));
                        phase.set(Phase::Failed(message));
                        return;
                    }
                };
                let bytes = js_sys::Uint8Array::to_vec(&bytes);
                let length = bytes.len();
                let saved = head.saved.as_deref().and_then(saved_locator);
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
                            Action::Relayout => {
                                if let Engine::Epub(view) = &c.engine {
                                    view.relayout();
                                }
                            }
                        }
                    }
                };
                let opened = open_book(
                    outbox,
                    kept,
                    id,
                    saved,
                    *settings.peek(),
                    fraction,
                    page,
                    alive.clone(),
                    bytes,
                    handler,
                )
                .await;
                match opened {
                    Ok(o) if alive.get() => {
                        let at = o.core.engine.locator();
                        *weak.borrow_mut() = Rc::downgrade(&o.core);
                        toc.set(o.toc);
                        *core.borrow_mut() = Some(o.core);
                        phase.set(Phase::Reading);
                        if kept {
                            check_place_later(id, live_api, alive.clone(), core.clone());
                        }
                        platform::info(&format!(
                            "pharos: reader opened item={id} format=epub from={from} bytes={length} ms={:.0} resumed={} at={}",
                            platform::now_ms() - began,
                            saved.is_some(),
                            at
                        ));
                    }
                    Ok(o) => o.core.engine.unmount(),
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
            let keys = KeyWatch::new(move |press| {
                if !key_alive.get() || platform::dialog_open() {
                    return false;
                }
                let held = key_core.borrow().clone();
                // A fit-width PDF also takes the vertical arrows, so the
                // press is read per engine; Escape is common.
                let pdf_press = match held.as_ref().map(|c| &c.engine) {
                    Some(Engine::Pdf(pdf)) => Some(pdf_key(press, pdf.fit(), pdf.host_scroll())),
                    _ => None,
                };
                let action = key_action(press);
                if action.is_none() && pdf_press.flatten().is_none() {
                    return false;
                }
                if action == Some(KeyAction::Escape) {
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
                let Some(c) = held else {
                    return false;
                };
                if let Some(Some(PdfKey::Scroll(dy))) = pdf_press {
                    if let Engine::Pdf(pdf) = &c.engine {
                        pdf.scroll_by(dy);
                    }
                    return true;
                }
                let forward = match pdf_press {
                    Some(Some(PdfKey::Turn(forward))) => forward,
                    Some(_) => return false,
                    None => action == Some(KeyAction::Next),
                };
                platform::spawn_detached(async move {
                    c.turn(forward).await;
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
                c.engine.unmount();
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
                c.apply_settings(next);
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

    let offline = {
        let _ = outbox.changed.read();
        load_pending().contains(id)
    };
    let current = *settings.read();
    let page_background = view_settings(current).theme.background;
    let shown = phase.read().clone();
    let label = progress_label(*fraction.read(), *page.read());
    let width = format!("width: {}%", super::reader::percent(*fraction.read()));
    let open = *panel.read();
    let pdf = *is_pdf.read();
    let host_class = if pdf {
        "pharos-reader-host pharos-reader-host-pdf"
    } else {
        "pharos-reader-host"
    };
    let host_font = if pdf {
        String::new()
    } else {
        format!("font-size: {}px", current.font_px)
    };
    let tap = EventHandler::new({
        let (core, swiped) = (core.clone(), swiped.clone());
        move |x: f64| {
            if swiped.replace(false) {
                return;
            }
            let Some(action) = tap_action(x) else {
                return;
            };
            let held = core.borrow().clone();
            if let Some(c) = held {
                platform::spawn_detached(async move {
                    c.turn(action == KeyAction::Next).await;
                });
            }
        }
    });
    let swipe = EventHandler::new({
        let (core, swiped) = (core.clone(), swiped.clone());
        move |dx: f64| {
            let Some(action) = swipe_action(dx) else {
                return;
            };
            swiped.set(true);
            let held = core.borrow().clone();
            if let Some(c) = held {
                platform::spawn_detached(async move {
                    c.turn(action == KeyAction::Next).await;
                });
            }
        }
    });
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
                if pdf {
                    button {
                        class: "pharos-control",
                        r#type: "button",
                        aria_label: "Fit width",
                        aria_pressed: "{current.pdf_fit == PdfFit::Width}",
                        onclick: move |_| apply.call(ReaderSettings { pdf_fit: current.pdf_fit.toggled(), ..current }),
                        "{PdfFit::Width.label()}"
                    }
                } else {
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
            }
            // The page theme fills the stage; the host, one measure wide at
            // the text size, sits in its middle.
            div { class: "pharos-reader-stage", style: "background: {page_background}",
                div {
                    class: host_class,
                    id: HOST_ID,
                    style: "{host_font}",
                    onpointerdown: {
                        let swipe_from = swipe_from.clone();
                        let swiped = swiped.clone();
                        move |e| {
                            // Chrome sends no click after a touch that moved,
                            // so a swipe's flag must not outlive the next press.
                            swiped.set(false);
                            swipe_from.set(e.client_coordinates().x);
                        }
                    },
                    onpointerup: {
                        let swipe_from = swipe_from.clone();
                        move |e| {
                            // A mouse drag is not a swipe, as in quire-web.
                            let dx = e.client_coordinates().x - swipe_from.replace(f64::NAN);
                            if pdf && e.pointer_type() != "mouse" {
                                swipe.call(dx);
                            }
                        }
                    },
                    onclick: move |e| {
                        if !pdf {
                            return;
                        }
                        let Some(host) = web_sys::window()
                            .and_then(|w| w.document())
                            .and_then(|d| d.get_element_by_id(HOST_ID))
                        else {
                            return;
                        };
                        let rect = host.get_bounding_client_rect();
                        tap.call((e.client_coordinates().x - rect.left()) / rect.width().max(1.0));
                    },
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
                if offline {
                    span { class: "pharos-reader-offline", role: "status", "{OFFLINE_NOTE}" }
                }
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
