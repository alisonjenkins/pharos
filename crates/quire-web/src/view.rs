//! A book drawn into a shadow root, one CSS column per page.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use quire::{Content, El, Locator, PageSize, Publication, Target, Value};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, BlobPropertyBag, Document, Element, Event, EventTarget, HtmlElement, HtmlImageElement,
    MouseEvent, PointerEvent, Range, ResizeObserver, ShadowRoot, ShadowRootInit, ShadowRootMode,
    Text, Url,
};

use crate::layout::{self, Columns, Fit};
use crate::settings::Settings;

/// What a click, swipe or resize inside the view asks for. The view never
/// acts on these itself; the caller decides (and may ignore them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Next,
    Prev,
    /// A link inside the book was followed.
    Jump(Target),
    /// The view changed size; call [`View::relayout`].
    Relayout,
}

/// Where in a section to open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    Beginning,
    End,
    /// A [`Locator::offset`].
    Offset(u32),
    /// An element id.
    Fragment(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error(transparent)]
    Section(#[from] quire::SectionError),
    #[error("browser: {0}")]
    Js(String),
}

impl From<JsValue> for ViewError {
    fn from(v: JsValue) -> Self {
        ViewError::Js(v.as_string().unwrap_or_else(|| format!("{v:?}")))
    }
}

type Actions = Rc<RefCell<Option<Rc<dyn Fn(Action)>>>>;
type Listener = (&'static str, Closure<dyn FnMut(Event)>);
type Observer = (ResizeObserver, Closure<dyn FnMut()>);

/// A swipe shorter than this is a tap.
const SWIPE_PX: f64 = 40.0;

/// Space kept around a fixed-layout page. Small, because such a page carries
/// its own margins and every pixel of a phone screen counts.
const FIXED_GUTTER_PX: f64 = 8.0;

/// A handle to a mounted book; clones share it. Dropping the last one, or
/// [`View::unmount`], removes its nodes, listeners, observer and `blob:` URLs.
#[derive(Clone)]
pub struct View(Rc<Inner>);

/// A [`View`] that does not keep it alive, for the [`View::on_action`]
/// callback to hold (a `View` there would keep itself alive forever).
#[derive(Clone)]
pub struct WeakView(std::rc::Weak<Inner>);

impl WeakView {
    pub fn upgrade(&self) -> Option<View> {
        self.0.upgrade().map(View)
    }
}

struct Inner {
    book: Rc<Publication>,
    document: Document,
    host: Element,
    shadow: ShadowRoot,
    sheets: HtmlElement,
    frame: HtmlElement,
    strip: HtmlElement,
    settings: RefCell<Settings>,
    state: RefCell<State>,
    blobs: RefCell<HashMap<String, Option<String>>>,
    actions: Actions,
    listeners: RefCell<Vec<Listener>>,
    observer: RefCell<Option<Observer>>,
    /// Bumped by every [`View::open`]; an open that finds it changed after
    /// waiting for images was superseded and stops.
    generation: Cell<u64>,
    opening: Cell<bool>,
}

#[derive(Default)]
struct State {
    spine: usize,
    page: u32,
    pages: u32,
    /// The reader's place: the first character of the page they turned to,
    /// or the exact place they opened. A relayout shows the page holding it
    /// and does not move it, so repeated relayouts cannot drift.
    anchor: usize,
    columns: Option<Columns>,
    nodes: Vec<Text>,
    starts: Vec<usize>,
    lens: Vec<usize>,
    len: usize,
    /// The section is a fixed-layout page of this size, drawn whole.
    fixed: Option<PageSize>,
}

impl View {
    /// Mounts `book` into an open shadow root on `host`, replacing anything
    /// a previous view left there. The host's size is the page size; style
    /// it from outside. Nothing is shown until [`View::open`].
    pub fn mount(
        host: &Element,
        book: Rc<Publication>,
        settings: Settings,
    ) -> Result<View, ViewError> {
        let document = host
            .owner_document()
            .ok_or_else(|| ViewError::Js("host is not in a document".into()))?;
        let shadow = match host.shadow_root() {
            Some(s) => s,
            None => host.attach_shadow(&ShadowRootInit::new(ShadowRootMode::Open))?,
        };
        shadow.set_inner_html("");
        let sheets = div(&document, "quire-sheets")?;
        let frame = div(&document, "quire-frame")?;
        let strip = div(&document, "quire-strip")?;
        frame.append_child(&strip)?;
        shadow.append_child(&sheets)?;
        shadow.append_child(&frame)?;

        let inner = Rc::new(Inner {
            book,
            document,
            host: host.clone(),
            shadow,
            sheets,
            frame,
            strip,
            settings: RefCell::new(settings),
            state: RefCell::new(State::default()),
            blobs: RefCell::new(HashMap::new()),
            actions: Rc::new(RefCell::new(None)),
            listeners: RefCell::new(Vec::new()),
            observer: RefCell::new(None),
            generation: Cell::new(0),
            opening: Cell::new(false),
        });
        inner.listen()?;
        inner.observe()?;
        Ok(View(inner))
    }

    /// Where clicks, swipes, link follows and resizes are reported. A
    /// callback that needs the view should capture [`View::downgrade`].
    pub fn on_action(&self, f: impl Fn(Action) + 'static) {
        *self.0.actions.borrow_mut() = Some(Rc::new(f));
    }

    pub fn downgrade(&self) -> WeakView {
        WeakView(Rc::downgrade(&self.0))
    }

    /// Removes the view now, as dropping its last handle would.
    pub fn unmount(&self) {
        self.0.teardown();
    }

    pub fn book(&self) -> &Publication {
        &self.0.book
    }

    /// Opens reading-order item `spine` at `start`. Waits for the section's
    /// images to decode, since they move text between pages.
    /// A later `open` (or a page turn across sections) supersedes one still
    /// waiting for images; page turns made meanwhile are ignored.
    pub async fn open(&self, spine: usize, start: Start) -> Result<(), ViewError> {
        let generation = self.0.generation.get().wrapping_add(1);
        self.0.generation.set(generation);
        self.0.opening.set(true);
        let result = self.open_now(spine, start, generation).await;
        if self.0.generation.get() == generation {
            self.0.opening.set(false);
        }
        result
    }

    async fn open_now(&self, spine: usize, start: Start, generation: u64) -> Result<(), ViewError> {
        let section = self.0.book.section(spine)?;
        self.0.build(&section, spine)?;
        self.0.decode_images().await;
        if self.0.generation.get() != generation {
            return Ok(());
        }
        self.0.apply_settings();
        self.0.measure();
        let (page, anchor) = match start {
            Start::Beginning => (0, Some(0)),
            Start::End => (self.pages().saturating_sub(1), None),
            Start::Offset(o) => (self.0.page_of_offset(o as usize), Some(o as usize)),
            Start::Fragment(id) => (self.0.page_of_fragment(&id), None),
        };
        self.0.show(page, anchor);
        Ok(())
    }

    /// Opens the place a locator names.
    pub async fn go_to(&self, at: Locator) -> Result<(), ViewError> {
        self.open(at.spine as usize, Start::Offset(at.offset)).await
    }

    /// Opens a link or table-of-contents target.
    pub async fn go(&self, target: &Target) -> Result<(), ViewError> {
        let start = target
            .fragment
            .clone()
            .map_or(Start::Beginning, Start::Fragment);
        self.open(target.spine, start).await
    }

    /// The next page, crossing into the next linear section at a section's
    /// end. `false` at the end of the book.
    pub async fn next(&self) -> Result<bool, ViewError> {
        if self.0.opening.get() {
            return Ok(true);
        }
        let (page, pages, spine) = self.0.position();
        if page.saturating_add(1) < pages {
            self.0.show(page.saturating_add(1), None);
            return Ok(true);
        }
        let following = self
            .0
            .book
            .spine()
            .iter()
            .enumerate()
            .skip(spine.saturating_add(1))
            .find(|(_, s)| s.linear)
            .map(|(i, _)| i);
        match following {
            Some(i) => self.open(i, Start::Beginning).await.map(|()| true),
            None => Ok(false),
        }
    }

    /// The previous page, crossing back to the previous linear section's last
    /// page. `false` at the start of the book.
    pub async fn prev(&self) -> Result<bool, ViewError> {
        if self.0.opening.get() {
            return Ok(true);
        }
        let (page, _, spine) = self.0.position();
        if page > 0 {
            self.0.show(page.saturating_sub(1), None);
            return Ok(true);
        }
        let before = self
            .0
            .book
            .spine()
            .iter()
            .enumerate()
            .take(spine)
            .rev()
            .find(|(_, s)| s.linear)
            .map(|(i, _)| i);
        match before {
            Some(i) => self.open(i, Start::End).await.map(|()| true),
            None => Ok(false),
        }
    }

    /// Lays the section out again (after a resize) and shows the page
    /// holding the reader's place. Does nothing while the host has no size
    /// (`display: none`), so hiding the reader cannot lose the place.
    pub fn relayout(&self) {
        if !self.0.has_size() || self.0.opening.get() {
            return;
        }
        let anchor = self.0.state.borrow().anchor;
        self.0.apply_settings();
        self.0.measure();
        self.0.show(self.0.page_of_offset(anchor), Some(anchor));
    }

    /// Changes the reader settings, keeping the reader's place.
    pub fn set_settings(&self, settings: Settings) {
        *self.0.settings.borrow_mut() = settings;
        self.relayout();
    }

    /// The reader's place: the first character of the page last turned to,
    /// or the place last opened, unchanged by relayouts.
    pub fn locator(&self) -> Locator {
        let s = self.0.state.borrow();
        Locator {
            spine: u32::try_from(s.spine).unwrap_or(u32::MAX),
            offset: u32::try_from(s.anchor).unwrap_or(u32::MAX),
        }
    }

    /// The first character drawn on the page shown, measured now.
    pub fn first_visible(&self) -> u32 {
        u32::try_from(self.0.first_visible()).unwrap_or(u32::MAX)
    }

    /// How far through the whole book the page shown starts, from 0 to 1.
    pub fn progress(&self) -> f64 {
        let at = self.locator();
        let len = self.0.state.borrow().len;
        let within = if len == 0 {
            0.0
        } else {
            f64::from(at.offset) / len as f64
        };
        self.0.book.progress(at.spine as usize, within)
    }

    /// The page of the current section that holds `offset`.
    pub fn page_of(&self, offset: u32) -> u32 {
        self.0.page_of_offset(offset as usize)
    }

    /// The page shown in the current section, from 0.
    pub fn page(&self) -> u32 {
        self.0.state.borrow().page
    }

    /// Pages in the current section.
    pub fn pages(&self) -> u32 {
        self.0.state.borrow().pages
    }

    /// The reading-order index shown.
    pub fn spine(&self) -> usize {
        self.0.state.borrow().spine
    }
}

impl Inner {
    fn position(&self) -> (u32, u32, usize) {
        let s = self.state.borrow();
        (s.page, s.pages, s.spine)
    }

    fn emit(actions: &Actions, a: Action) {
        let f = actions.borrow().clone();
        if let Some(f) = f {
            f(a);
        }
    }

    fn listen(&self) -> Result<(), ViewError> {
        let target: &EventTarget = self.shadow.as_ref();
        let down_x = Rc::new(Cell::new(f64::NAN));
        let swiped = Rc::new(Cell::new(false));

        let (dx, sw) = (down_x.clone(), swiped.clone());
        self.add(target, "pointerdown", move |e| {
            if let Some(p) = e.dyn_ref::<PointerEvent>() {
                dx.set(f64::from(p.client_x()));
                sw.set(false);
            }
        })?;

        let (actions, dx, sw) = (self.actions.clone(), down_x, swiped.clone());
        self.add(target, "pointerup", move |e| {
            let Some(p) = e.dyn_ref::<PointerEvent>() else {
                return;
            };
            // A mouse drag selects text; only touch and pen swipe.
            if p.pointer_type() == "mouse" {
                dx.set(f64::NAN);
                return;
            }
            let moved = f64::from(p.client_x()) - dx.get();
            dx.set(f64::NAN);
            if moved.abs() >= SWIPE_PX {
                sw.set(true);
                Inner::emit(
                    &actions,
                    if moved < 0.0 {
                        Action::Next
                    } else {
                        Action::Prev
                    },
                );
            }
        })?;

        let (actions, host) = (self.actions.clone(), self.host.clone());
        self.add(target, "click", move |e| {
            if swiped.replace(false) {
                return;
            }
            // A click that ends a text selection is not a page tap.
            let selecting = web_sys::window()
                .and_then(|w| w.get_selection().ok().flatten())
                .is_some_and(|s| !s.is_collapsed());
            if selecting {
                return;
            }
            let Some(el) = e.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
                return;
            };
            if let Ok(Some(link)) = el.closest("[data-quire-link]") {
                e.prevent_default();
                if let Some(t) = link
                    .get_attribute("data-quire-link")
                    .and_then(|v| v.parse::<Target>().ok())
                {
                    Inner::emit(&actions, Action::Jump(t));
                }
                return;
            }
            if let Ok(Some(_)) = el.closest("a[href]") {
                return;
            }
            let Some(m) = e.dyn_ref::<MouseEvent>() else {
                return;
            };
            let rect = host.get_bounding_client_rect();
            let x = (f64::from(m.client_x()) - rect.left()) / rect.width().max(1.0);
            if x < 1.0 / 3.0 {
                Inner::emit(&actions, Action::Prev);
            } else if x > 2.0 / 3.0 {
                Inner::emit(&actions, Action::Next);
            }
        })?;
        Ok(())
    }

    fn add(
        &self,
        target: &EventTarget,
        name: &'static str,
        f: impl FnMut(Event) + 'static,
    ) -> Result<(), ViewError> {
        let closure = Closure::<dyn FnMut(Event)>::new(f);
        target.add_event_listener_with_callback(name, closure.as_ref().unchecked_ref())?;
        self.listeners.borrow_mut().push((name, closure));
        Ok(())
    }

    fn observe(&self) -> Result<(), ViewError> {
        let actions = self.actions.clone();
        // Every report counts, the first included: a host shown before that
        // first report arrives gets its size only there. `relayout` ignores
        // a zero-size host and a section still opening.
        let closure = Closure::<dyn FnMut()>::new(move || {
            Inner::emit(&actions, Action::Relayout);
        });
        let observer = ResizeObserver::new(closure.as_ref().unchecked_ref())?;
        observer.observe(&self.host);
        *self.observer.borrow_mut() = Some((observer, closure));
        Ok(())
    }

    fn resource_url(&self, path: &str) -> Option<String> {
        if let Some(hit) = self.blobs.borrow().get(path) {
            return hit.clone();
        }
        let url = self.book.resource(path).ok().and_then(|r| {
            let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(r.bytes.as_slice()));
            let opts = BlobPropertyBag::new();
            opts.set_type(&r.media_type);
            let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &opts).ok()?;
            Url::create_object_url_with_blob(&blob).ok()
        });
        self.blobs.borrow_mut().insert(path.to_owned(), url.clone());
        url
    }

    fn build(&self, section: &quire::Section, spine: usize) -> Result<(), ViewError> {
        self.sheets.set_inner_html("");
        self.strip.set_inner_html("");
        for css in &section.styles {
            let style = self.document.create_element("style")?;
            style.set_text_content(Some(&css.render(&mut |p| self.resource_url(p))));
            self.sheets.append_child(&style)?;
        }
        let ours = self.document.create_element("style")?;
        ours.set_attribute("data-quire-settings", "")?;
        self.sheets.append_child(&ours)?;

        let mut state = State::default();
        let root = self.element(&section.root, &mut state)?;
        self.strip.append_child(&root)?;
        state.spine = spine;
        state.fixed = section.page;
        *self.state.borrow_mut() = state;
        Ok(())
    }

    /// Builds DOM nodes from the sanitised tree, one text node per text run
    /// in the same order as [`quire::Section::text`], so offsets agree.
    fn element(&self, el: &El, state: &mut State) -> Result<Element, ViewError> {
        let node = self
            .document
            .create_element_ns(Some(el.ns.uri()), &el.name)?;
        for a in &el.attrs {
            let value = match &a.value {
                Value::Text(t) => Some(t.clone()),
                Value::Resource(p) => self.resource_url(p),
                Value::Style(css) => Some(css.render(&mut |p| self.resource_url(p))),
                Value::Link(t) => Some(t.encode()),
            };
            if let Some(v) = value {
                node.set_attribute(&a.name, &v)?;
            }
        }
        for c in &el.children {
            match c {
                Content::Text(t) => {
                    let text = self.document.create_text_node(t);
                    let len = t.encode_utf16().count();
                    state.starts.push(state.len);
                    state.lens.push(len);
                    state.len = state.len.saturating_add(len);
                    node.append_child(&text)?;
                    state.nodes.push(text);
                }
                Content::Element(child) => {
                    let child = self.element(child, state)?;
                    node.append_child(&child)?;
                }
            }
        }
        Ok(node)
    }

    async fn decode_images(&self) {
        let Ok(list) = self.strip.query_selector_all("img") else {
            return;
        };
        for i in 0..list.length() {
            if let Some(img) = list
                .item(i)
                .and_then(|n| n.dyn_into::<HtmlImageElement>().ok())
            {
                // A broken image is drawn as nothing; it must not stop the page.
                let _ = JsFuture::from(img.decode()).await;
            }
        }
    }

    fn apply_settings(&self) {
        let s = self.settings.borrow();
        let fixed = self.state.borrow().fixed;
        if let Some(page) = fixed {
            // The host, not the frame: the frame takes its size from this
            // stylesheet, which may not have been written yet.
            let frame = (
                f64::from(self.host.client_width()),
                f64::from(self.host.client_height()),
            );
            let page = (f64::from(page.width), f64::from(page.height));
            let css = s.fixed_css(page, Fit::of(page, frame, FIXED_GUTTER_PX));
            if let Ok(Some(el)) = self.sheets.query_selector("[data-quire-settings]") {
                el.set_text_content(Some(&css));
            }
            // One page, never translated: the strip is the page.
            self.state.borrow_mut().columns = Some(Columns {
                width: frame.0.max(1.0),
                gap: 0.0,
            });
            return;
        }
        let width = f64::from(self.frame.client_width()) - 2.0 * s.margin_px;
        let height = f64::from(self.frame.client_height()) - 2.0 * s.margin_px;
        let columns = Columns {
            width: width.max(1.0),
            gap: 2.0 * s.margin_px,
        };
        let css = s.css(columns.width, columns.gap, height.max(1.0));
        if let Ok(Some(el)) = self.sheets.query_selector("[data-quire-settings]") {
            el.set_text_content(Some(&css));
        }
        self.state.borrow_mut().columns = Some(columns);
    }

    fn measure(&self) {
        if self.state.borrow().fixed.is_some() {
            self.state.borrow_mut().pages = 1;
            return;
        }
        let pages = self
            .state
            .borrow()
            .columns
            .map_or(1, |c| c.pages(f64::from(self.strip.scroll_width())));
        self.state.borrow_mut().pages = pages;
    }

    fn has_size(&self) -> bool {
        self.frame.client_width() > 0 && self.frame.client_height() > 0
    }

    /// Shows `page` and sets the reader's place to `anchor`, or to the
    /// page's first character when none is given.
    fn show(&self, page: u32, anchor: Option<usize>) {
        let (page, offset) = {
            let mut s = self.state.borrow_mut();
            let page = page.min(s.pages.saturating_sub(1));
            s.page = page;
            (page, s.columns.map_or(0.0, |c| c.offset(page)))
        };
        let _ = self
            .strip
            .style()
            .set_property("transform", &format!("translateX({offset}px)"));
        let _ = self.strip.set_attribute("data-page", &page.to_string());
        let anchor = anchor.unwrap_or_else(|| self.first_visible());
        self.state.borrow_mut().anchor = anchor;
    }

    /// Left edge of a range's first non-empty box, from the strip's left
    /// edge (the same with or without the page transform).
    fn x_of(&self, range: &Range) -> Option<f64> {
        let origin = self.strip.get_bounding_client_rect().left();
        let rects = range.get_client_rects()?;
        (0..rects.length())
            .filter_map(|i| rects.item(i))
            .find(|r| r.width() > 0.0 || r.height() > 0.0)
            .map(|r| r.left() - origin)
    }

    fn char_x(&self, node: &Text, at: usize, len: usize) -> Option<f64> {
        if at >= len {
            return None;
        }
        let range = self.document.create_range().ok()?;
        range.set_start(node, u32::try_from(at).ok()?).ok()?;
        range
            .set_end(node, u32::try_from(at.saturating_add(1)).ok()?)
            .ok()?;
        self.x_of(&range)
    }

    fn page_of_offset(&self, offset: usize) -> u32 {
        let s = self.state.borrow();
        let Some(columns) = s.columns else { return 0 };
        let Some((mut i, mut at)) = layout::node_at(&s.starts, &s.lens, offset) else {
            return 0;
        };
        // Collapsed whitespace has no box; the next character that has one
        // is on the same page or the next.
        while let (Some(node), Some(&len)) = (s.nodes.get(i), s.lens.get(i)) {
            while at < len {
                if let Some(x) = self.char_x(node, at, len) {
                    return columns.page_at(x);
                }
                at = at.saturating_add(1);
            }
            i = i.saturating_add(1);
            at = 0;
        }
        s.pages.saturating_sub(1)
    }

    fn page_of_fragment(&self, id: &str) -> u32 {
        let s = self.state.borrow();
        let (Some(columns), Some(el)) = (s.columns, self.shadow.get_element_by_id(id)) else {
            return 0;
        };
        let origin = self.strip.get_bounding_client_rect().left();
        columns.page_at(el.get_bounding_client_rect().left() - origin)
    }

    fn first_visible(&self) -> usize {
        let s = self.state.borrow();
        let Some(columns) = s.columns else { return 0 };
        let lo = f64::from(s.page) * columns.stride();
        let on_page_or_later = |x: f64| x >= lo - 0.5;
        for (i, node) in s.nodes.iter().enumerate() {
            let (Some(&start), Some(&len)) = (s.starts.get(i), s.lens.get(i)) else {
                break;
            };
            if len == 0 {
                continue;
            }
            // Does this node reach this page? Its last character's box says.
            let last = (0..len).rev().find_map(|k| self.char_x(node, k, len));
            if !last.is_some_and(on_page_or_later) {
                continue;
            }
            let k = layout::first_true(len, |k| {
                self.char_x(node, k, len).is_some_and(on_page_or_later)
            });
            return start.saturating_add(k);
        }
        s.len
    }
}

impl Inner {
    fn teardown(&self) {
        *self.actions.borrow_mut() = None;
        let target: &EventTarget = self.shadow.as_ref();
        for (name, closure) in self.listeners.borrow_mut().drain(..) {
            let _ =
                target.remove_event_listener_with_callback(name, closure.as_ref().unchecked_ref());
        }
        if let Some((observer, _)) = self.observer.borrow_mut().take() {
            observer.disconnect();
        }
        for url in self.blobs.borrow_mut().drain().filter_map(|(_, u)| u) {
            let _ = Url::revoke_object_url(&url);
        }
        // Only this view's own nodes: a newer view may share the shadow root.
        self.sheets.remove();
        self.frame.remove();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.teardown();
    }
}

fn div(document: &Document, class: &str) -> Result<HtmlElement, ViewError> {
    let el = document.create_element("div")?;
    el.set_class_name(class);
    el.dyn_into::<HtmlElement>()
        .map_err(|_| ViewError::Js("div is not an HtmlElement".into()))
}
