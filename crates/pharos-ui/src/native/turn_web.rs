//! The episode turn's layer (V790): a fixed overlay outside the player, so
//! it lives through the swap from one player to the next and is never an
//! ancestor of the `<video>`. Only `transform` and `opacity` change, by CSS
//! transition, so a TV or headset GPU runs it without repainting. The layer
//! is removed once it has faded into the video, leaving playback exactly as
//! with the turn off. Rules (timings, bar, captions) are in `turn.rs`.

use std::cell::RefCell;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use web_sys::{Document, Element, Event, HtmlCanvasElement, HtmlElement, HtmlVideoElement};

use super::platform::{info, now_ms, warn};
use super::turn::{face_transform, Direction, Step, Turn, FADE_MS, GIVE_UP_MS, START_DEG, TURN_MS};
use super::watch_view::VIDEO_ID;

/// The old episode's still is drawn no wider than this: enough for a face
/// that is turning away, cheap to draw from a 4K frame on a TV.
const SNAPSHOT_MAX_WIDTH: u32 = 640;
/// How often the caption and the give-up deadline are looked at.
const TICK_MS: i32 = 250;
/// Quick off the press, so it answers at once, then easing into flat.
const TURN_EASE: &str = "cubic-bezier(.2,.7,.2,1)";
const FADE_EASE: &str = "cubic-bezier(.4,0,.2,1)";
/// The `<html>` attribute that hides the player's own spinner while the
/// layer shows the load.
const TURNING_ATTR: &str = "data-turning";

type Listener = Closure<dyn FnMut(Event)>;

struct Active {
    item: String,
    turn: Turn,
    fading: bool,
    waiting_shown: bool,
    layer: HtmlElement,
    fill: HtmlElement,
    caption: Element,
    title: Element,
    still: Element,
    video: Option<(HtmlVideoElement, Vec<(&'static str, Listener)>)>,
    tick: Option<(i32, Closure<dyn FnMut()>)>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    /// The item a navigation asked to open with a turn, once its detail
    /// says it is a video.
    static EXPECTED: RefCell<Option<(String, Direction)>> = const { RefCell::new(None) };
    /// Listeners of a finished turn, dropped at the next begin rather than
    /// from inside one of their own calls.
    static RETIRED: RefCell<Vec<Box<dyn std::any::Any>>> = const { RefCell::new(Vec::new()) };
}

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = CanvasRenderingContext2D)]
    type Ctx2d;
    /// `drawImage(image, sx, sy, sw, sh, dx, dy, dw, dh)`: `source` is the
    /// part of the frame drawn, `x`..`h` where.
    #[wasm_bindgen(method, catch, js_name = drawImage)]
    #[allow(clippy::too_many_arguments)]
    fn draw_image_part(
        this: &Ctx2d,
        image: &HtmlVideoElement,
        sx: f64,
        sy: f64,
        sw: f64,
        sh: f64,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> Result<(), JsValue>;
}

fn document() -> Option<Document> {
    web_sys::window().and_then(|w| w.document())
}

/// Start a turn from the episode on screen to `item`. Only a video with a
/// frame showing can turn away; anything else (a song, a player still
/// loading) leaves `item` to [`turn_open`] once it is known to be a video.
pub fn turn_begin(item: &str, dir: Direction) -> bool {
    let snapshot = document()
        .and_then(|d| d.get_element_by_id(VIDEO_ID))
        .and_then(|e| e.dyn_into::<HtmlVideoElement>().ok())
        .filter(|v| v.ready_state() >= 2 && v.video_width() > 0)
        .and_then(|v| snapshot(&v));
    match snapshot {
        Some(canvas) => begin(item, dir, Some(canvas), "next"),
        None => {
            EXPECTED.with(|e| *e.borrow_mut() = Some((item.to_owned(), dir)));
            false
        }
    }
}

/// A navigation opens `item`: turn onto it once its detail says it is a video.
pub fn turn_expect(item: &str, dir: Direction) {
    EXPECTED.with(|e| *e.borrow_mut() = Some((item.to_owned(), dir)));
}

/// `item`, which a navigation expected, is a video: turn onto it.
pub fn turn_open(item: &str) {
    let expected = EXPECTED.with(|e| {
        let mut e = e.borrow_mut();
        match e.as_ref() {
            Some((id, _)) if id == item => e.take(),
            _ => None,
        }
    });
    if let Some((_, dir)) = expected {
        begin(item, dir, None, "open");
    }
}

/// `item`'s playback is negotiated: show its still and name on the face,
/// and hook its `<video>` for the rest of the load.
pub fn turn_ready(item: &str, image: Option<&str>, title: &str) {
    let found = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let Some(active) = a.as_mut().filter(|a| a.item == item) else {
            return false;
        };
        if let Some(src) = image {
            let _ = active.still.set_attribute("src", src);
        }
        active.title.set_text_content(Some(title));
        true
    });
    if !found {
        return;
    }
    hear(item, Step::Negotiated);
    attach(item);
}

/// Stop `item`'s turn (or the wait for one) at once: an error, the page
/// left, the player made small.
pub fn turn_cancel(item: &str, reason: &str) {
    EXPECTED.with(|e| {
        let mut e = e.borrow_mut();
        if e.as_ref().is_some_and(|(id, _)| id == item) {
            e.take();
        }
    });
    let matches = ACTIVE.with(|a| a.borrow().as_ref().is_some_and(|a| a.item == item));
    if matches {
        finish(reason);
    }
}

/// Stop whatever turn is running: the engine playing it hit a fatal error.
pub fn turn_cancel_current(reason: &str) {
    if ACTIVE.with(|a| a.borrow().is_some()) {
        finish(reason);
    }
}

fn begin(item: &str, dir: Direction, outgoing: Option<HtmlCanvasElement>, from: &str) -> bool {
    let Some(doc) = document() else {
        return false;
    };
    // The F key's fullscreen is on the player, which this swap replaces;
    // a layer outside it would not show.
    let page_or_none = match (doc.fullscreen_element(), doc.document_element()) {
        (None, _) => true,
        (Some(fs), Some(root)) => fs == root,
        (Some(_), None) => false,
    };
    if !page_or_none {
        info(&format!(
            "pharos: turn skipped item={item} reason=player-fullscreen"
        ));
        return false;
    }
    if ACTIVE.with(|a| a.borrow().is_some()) {
        finish("replaced");
    }
    RETIRED.with(|r| r.borrow_mut().clear());
    let Some(layer) = build(&doc, outgoing, dir) else {
        warn(&format!(
            "pharos: turn skipped item={item} reason=layer-not-built"
        ));
        return false;
    };
    let (layer, face, fill, caption, title, still) = layer;
    let started = now_ms();
    let tick = Closure::<dyn FnMut()>::new(tick);
    let tick_id = web_sys::window().and_then(|w| {
        w.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            TICK_MS,
        )
        .ok()
    });
    if let Some(root) = doc.document_element() {
        let _ = root.set_attribute(TURNING_ATTR, "");
    }
    // Into the first pose without a transition, then the whole turn in one
    // move; the load never holds it part way (B841).
    set_face(&face, START_DEG * dir.sign(), None);
    let _ = face.offset_width();
    set_face(&face, 0.0, Some(TURN_MS));
    after(TURN_MS, item, turned);
    ACTIVE.with(|a| {
        *a.borrow_mut() = Some(Active {
            item: item.to_owned(),
            turn: Turn::new(started),
            fading: false,
            waiting_shown: false,
            layer,
            fill,
            caption,
            title,
            still,
            video: None,
            tick: tick_id.map(|id| (id, tick)),
        });
    });
    info(&format!(
        "pharos: turn begin item={item} from={from} dir={}",
        match dir {
            Direction::Forward => "forward",
            Direction::Back => "back",
        }
    ));
    true
}

type Built = (
    HtmlElement,
    HtmlElement,
    HtmlElement,
    Element,
    Element,
    Element,
);

fn build(doc: &Document, outgoing: Option<HtmlCanvasElement>, dir: Direction) -> Option<Built> {
    let el = |tag: &str, class: &str| -> Option<Element> {
        let e = doc.create_element(tag).ok()?;
        e.set_class_name(class);
        Some(e)
    };
    let html = |e: Element| e.dyn_into::<HtmlElement>().ok();
    let layer = html(el("div", "pharos-turn")?)?;
    if let Some(canvas) = outgoing {
        let out = html(el("div", "pharos-turn-face is-out")?)?;
        canvas.set_class_name("pharos-turn-still");
        out.append_child(&canvas).ok()?;
        let shade = el("div", "pharos-turn-shade")?;
        out.append_child(&shade).ok()?;
        layer.append_child(&out).ok()?;
        set_face(&out, 0.0, None);
        let _ = out.offset_width();
        set_face(&out, -START_DEG * dir.sign(), Some(TURN_MS));
        let _ = out.class_list().add_1("is-gone");
    }
    let face = html(el("div", "pharos-turn-face is-in")?)?;
    let still = el("img", "pharos-turn-still")?;
    let _ = still.set_attribute("alt", "");
    let title = el("p", "pharos-turn-title")?;
    let caption = el("p", "pharos-turn-caption")?;
    let _ = caption.set_attribute("role", "status");
    let _ = caption.set_attribute("aria-live", "polite");
    let bar = el("div", "pharos-turn-bar")?;
    let fill = html(el("div", "pharos-turn-fill")?)?;
    bar.append_child(&fill).ok()?;
    for child in [
        still.clone(),
        el("div", "pharos-turn-shade")?,
        el("div", "pharos-turn-rim")?,
        title.clone(),
        caption.clone(),
        bar,
    ] {
        face.append_child(&child).ok()?;
    }
    layer.append_child(&face).ok()?;
    doc.body()?.append_child(&layer).ok()?;
    Some((layer, face, fill, caption, title, still))
}

fn snapshot(video: &HtmlVideoElement) -> Option<HtmlCanvasElement> {
    // A 3D version cropped to one eye turns as that eye (B961), and a
    // picture cropped out of baked-in bars as that picture (B1017).
    let (eye, (vw, vh)) = super::platform::picture_of(video);
    let w = vw.min(SNAPSHOT_MAX_WIDTH);
    let h = (u64::from(vh) * u64::from(w) / u64::from(vw.max(1))).max(1);
    let h = u32::try_from(h).ok()?;
    let canvas = document()?
        .create_element("canvas")
        .ok()?
        .dyn_into::<HtmlCanvasElement>()
        .ok()?;
    canvas.set_width(w);
    canvas.set_height(h);
    let ctx = canvas.get_context("2d").ok()??.unchecked_into::<Ctx2d>();
    let (fw, fh) = (
        f64::from(video.video_width()),
        f64::from(video.video_height()),
    );
    let drawn = ctx.draw_image_part(
        video,
        eye.x * fw,
        eye.y * fh,
        eye.w * fw,
        eye.h * fh,
        0.0,
        0.0,
        f64::from(w),
        f64::from(h),
    );
    if let Err(e) = drawn {
        warn(&format!("pharos: turn snapshot failed error={e:?}"));
        return None;
    }
    Some(canvas)
}

fn set_face(face: &HtmlElement, deg: f64, ms: Option<u32>) {
    let style = face.style();
    let transition = ms.map_or_else(
        || "none".to_owned(),
        |ms| format!("transform {ms}ms {TURN_EASE}"),
    );
    let _ = style.set_property("transition", &transition);
    let _ = style.set_property("transform", &face_transform(deg));
}

fn attach(item: &str) {
    let Some(video) = document()
        .and_then(|d| d.get_element_by_id(VIDEO_ID))
        .and_then(|e| e.dyn_into::<HtmlVideoElement>().ok())
    else {
        return;
    };
    let mut listeners: Vec<(&'static str, Listener)> = Vec::new();
    for kind in ["loadedmetadata", "progress", "loadeddata", "error"] {
        let id = item.to_owned();
        let target = video.clone();
        let f = Listener::new(move |_: Event| on_video(&id, kind, &target));
        let _ = video.add_event_listener_with_callback(kind, f.as_ref().unchecked_ref());
        listeners.push((kind, f));
    }
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut().filter(|a| a.item == item) {
            active.video = Some((video.clone(), listeners));
        }
    });
    // Events that fired before the hook.
    let state = video.ready_state();
    if state >= 2 {
        on_video(item, "loadeddata", &video);
    } else if state >= 1 {
        on_video(item, "loadedmetadata", &video);
    }
}

fn on_video(item: &str, kind: &str, video: &HtmlVideoElement) {
    match kind {
        "loadedmetadata" => hear(item, Step::Metadata),
        "progress" => hear(item, Step::Buffered(buffered_ahead_ms(video))),
        "loadeddata" => hear(item, Step::FirstFrame),
        "error" => turn_cancel(item, "video-error"),
        _ => {}
    }
}

/// How much media is buffered past the playback position, in ms.
fn buffered_ahead_ms(video: &HtmlVideoElement) -> u32 {
    let ranges = video.buffered();
    let at = video.current_time();
    let ahead = (0..ranges.length())
        .filter_map(|i| Some((ranges.start(i).ok()?, ranges.end(i).ok()?)))
        .find(|(s, e)| *s <= at + 0.5 && at <= *e)
        .map_or(0.0, |(_, e)| (e - at).max(0.0));
    let ms = (ahead * 1000.0).min(f64::from(u32::MAX));
    // Non-negative and in range after the clamps above.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = ms as u32;
    ms
}

/// Run `f(item)` after `ms`, if `item`'s turn is still the one running.
fn after(ms: u32, item: &str, f: fn(&str)) {
    let id = item.to_owned();
    let call = Closure::once_into_js(move || {
        if ACTIVE.with(|a| a.borrow().as_ref().is_some_and(|a| a.item == id)) {
            f(&id);
        }
    });
    if let Some(w) = web_sys::window() {
        let ms = i32::try_from(ms).unwrap_or(i32::MAX);
        let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(call.unchecked_ref(), ms);
    }
}

fn hear(item: &str, step: Step) {
    let fade = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let Some(active) = a.as_mut().filter(|a| a.item == item && !a.fading) else {
            return false;
        };
        let now = now_ms();
        let Some(done) = active.turn.hear(step, now) else {
            return false;
        };
        let _ = active
            .fill
            .style()
            .set_property("transform", &format!("scaleX({done:.3})"));
        active.turn.fades(now)
    });
    if fade {
        fade_out(item);
    }
}

/// The turn's one move is over: fade into the video if its first frame is
/// in, else show on the flat face that the load goes on.
fn turned(item: &str) {
    let fade = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let Some(active) = a.as_mut().filter(|a| !a.fading) else {
            return false;
        };
        // The timer says the turn is over, whatever the clock rounds to.
        if active.turn.step == Step::FirstFrame {
            return true;
        }
        if !active.waiting_shown {
            active.waiting_shown = true;
            let _ = active.layer.class_list().add_1("is-waiting");
        }
        false
    });
    if fade {
        fade_out(item);
    }
}

fn fade_out(item: &str) {
    let ms = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let active = a.as_mut().filter(|a| a.item == item && !a.fading)?;
        active.fading = true;
        let layer = active.layer.style();
        let _ = layer.set_property("transition", &format!("opacity {FADE_MS}ms {FADE_EASE}"));
        let _ = layer.set_property("opacity", "0");
        let _ = active.layer.class_list().remove_1("is-waiting");
        let _ = active.caption.class_list().remove_1("is-shown");
        Some(now_ms() - active.turn.begun_ms)
    });
    let Some(ms) = ms else {
        return;
    };
    info(&format!("pharos: turn fade item={item} after_ms={ms:.0}"));
    after(FADE_MS, item, |_| finish("landed"));
}

fn tick() {
    let give_up = ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        let active = a.as_mut().filter(|a| !a.fading)?;
        let now = now_ms();
        if active.turn.given_up(now) {
            return Some(active.item.clone());
        }
        match active.turn.caption(now) {
            Some(text) => {
                if active.caption.text_content().as_deref() != Some(text) {
                    active.caption.set_text_content(Some(text));
                }
                let _ = active.caption.class_list().add_1("is-shown");
            }
            None => {
                let _ = active.caption.class_list().remove_1("is-shown");
            }
        }
        None
    });
    if let Some(item) = give_up {
        warn(&format!(
            "pharos: turn given up item={item} after_ms={GIVE_UP_MS} reason=no-first-frame"
        ));
        finish("gave-up");
    }
}

fn finish(reason: &str) {
    let Some(active) = ACTIVE.with(|a| a.borrow_mut().take()) else {
        return;
    };
    active.layer.remove();
    if let Some(root) = document().and_then(|d| d.document_element()) {
        let _ = root.remove_attribute(TURNING_ATTR);
    }
    if let (Some((id, tick)), Some(w)) = (active.tick, web_sys::window()) {
        w.clear_interval_with_handle(id);
        RETIRED.with(|r| r.borrow_mut().push(Box::new(tick)));
    }
    if let Some((video, listeners)) = active.video {
        for (kind, f) in listeners {
            let _ = video.remove_event_listener_with_callback(kind, f.as_ref().unchecked_ref());
            RETIRED.with(|r| r.borrow_mut().push(Box::new(f)));
        }
    }
    let ms = now_ms() - active.turn.begun_ms;
    info(&format!(
        "pharos: turn end item={} reason={reason} after_ms={ms:.0}",
        active.item
    ));
}
