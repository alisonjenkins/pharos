//! Browser driver for the Rust subtitle renderer (spec 026 slice 1): the
//! canvas over the picture, the frame loop and the fallback.
//!
//! Cost model. While a video plays, one `requestVideoFrameCallback` per
//! presented frame (`requestAnimationFrame` where the browser lacks it) calls
//! `Renderer::frame`, a binary search that answers `changed == false` and
//! ends the callback: no layout read, no GL call, no allocation. A cue
//! change rasters once (cached), uploads the new masks and redraws the
//! canvas. The loop stops when the video pauses; seeks, a resize of the
//! player, the control bar showing or hiding and the size picker each kick
//! one frame by hand. Layout is read only on those events, never per frame.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gloo_net::http::Request;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use web_sys::{HtmlCanvasElement, HtmlElement, HtmlVideoElement};

use pharos_subs::Viewport;

use super::playback::SubtitleSize;
use super::subtitles::{
    content_box, controls_inset, style_for, FallbackReason, RustSubs, SubtitleRenderer,
    FIRST_SUBTITLE_BUDGET_MS,
};
use crate::render::{self, subtitles::SubtitleCompositor};

/// The bundled font (flake `pharosUiBundle`), fetched on the first text
/// subtitle only. CJK and symbol fallbacks are out of scope for slice 1.
const FONT_URL: &str = "/ui/assets/fonts/LiberationSans-Regular.ttf";
const CANVAS_ID: &str = "pharos-rust-subs";
const CANVAS_CLASS: &str = "pharos-rust-subs";
/// The renderer rasters at most 1920 px wide whatever the display; keep the
/// canvas no bigger than that too.
const MAX_BUFFER_WIDTH: f64 = 1920.0;
const MAX_PIXEL_RATIO: f64 = 2.0;

thread_local! {
    /// The viewer's size choice, set by `apply_subtitle_size`.
    static SIZE: Cell<SubtitleSize> = const { Cell::new(SubtitleSize::Medium) };
    /// Whether the player's control bar shows (set by the watch view).
    static CONTROLS_VISIBLE: Cell<bool> = const { Cell::new(true) };
    /// The one live driver, to kick when the size or controls change.
    static CURRENT: RefCell<Weak<RefCell<Core>>> = const { RefCell::new(Weak::new()) };
    /// The font file, kept once fetched.
    static FONT: RefCell<Option<Rc<Vec<u8>>>> = const { RefCell::new(None) };
}

enum Pending {
    Rvfc(f64),
    Raf(i32),
}

struct Core {
    video: HtmlVideoElement,
    canvas: HtmlCanvasElement,
    frame_box: HtmlElement,
    renderer: RustSubs,
    compositor: Box<dyn SubtitleCompositor>,
    viewport: Viewport,
    size_applied: SubtitleSize,
    measure_due: bool,
    pending: Option<Pending>,
    tick: Option<Closure<dyn FnMut(JsValue, JsValue)>>,
    rvfc: Option<js_sys::Function>,
    dead: bool,
    report: Rc<dyn Fn(FallbackReason)>,
}

fn now() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map_or(0.0, |p| p.now())
}

fn fallback_log(reason: FallbackReason, detail: &str) {
    super::platform::warn(&format!(
        "pharos: subtitles fallback reason={} detail={detail}",
        reason.label()
    ));
}

impl Core {
    /// Reads the player's layout into the viewport. Only on events.
    fn measure(&mut self) {
        self.measure_due = false;
        let css_w = f64::from(self.frame_box.client_width());
        let css_h = f64::from(self.frame_box.client_height());
        let dpr = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio());
        let mut ratio = dpr.clamp(0.5, MAX_PIXEL_RATIO);
        if css_w * ratio > MAX_BUFFER_WIDTH {
            ratio = MAX_BUFFER_WIDTH / css_w;
        }
        let w = (css_w * ratio).round().max(1.0) as u32;
        let h = (css_h * ratio).round().max(1.0) as u32;
        self.compositor.resize(w, h);
        let content = content_box(w, h, self.video.video_width(), self.video.video_height());
        let visible = CONTROLS_VISIBLE.with(Cell::get);
        let inset_css = if visible {
            let bottom = self.frame_box.get_bounding_client_rect().bottom();
            let top = self
                .frame_box
                .closest(".pharos-player")
                .ok()
                .flatten()
                .and_then(|p| p.query_selector(".pharos-controls").ok().flatten())
                .map_or(f64::NAN, |c| c.get_bounding_client_rect().top());
            controls_inset(true, bottom, top)
        } else {
            0
        };
        self.viewport = Viewport {
            width: w,
            height: h,
            content,
            bottom_inset: (f64::from(inset_css) * ratio).round() as u32,
        };
        let size = SIZE.with(Cell::get);
        if size != self.size_applied {
            self.size_applied = size;
            self.renderer.set_style(style_for(size));
        }
    }

    /// One frame: nothing at all unless the cues changed.
    fn step(&mut self, media_s: Option<f64>) {
        let started = now();
        if self.measure_due {
            self.measure();
        }
        let t_ms = (media_s.unwrap_or_else(|| self.video.current_time()) * 1000.0).round() as i64;
        let frame = self.renderer.frame(t_ms, &self.viewport);
        let drawn = if frame.changed {
            self.compositor.draw(&frame)
        } else if self.compositor.needs_redraw() {
            self.compositor.redraw()
        } else {
            return;
        };
        match drawn {
            Ok(()) => {
                // No client-event kind exists for renderer timings (the
                // events are a capnp enum), so the cue-change time is a
                // debug line until one is added (FR-014).
                if frame.changed {
                    web_sys::console::debug_1(
                        &format!(
                            "pharos: subtitles cue_change render_ms={:.2} masks={} t_ms={t_ms}",
                            now() - started,
                            frame.masks.len()
                        )
                        .into(),
                    );
                }
            }
            Err(f) => self.fail(FallbackReason::Gpu, f.label()),
        }
    }

    fn fail(&mut self, reason: FallbackReason, detail: &str) {
        if self.dead {
            return;
        }
        self.dead = true;
        fallback_log(reason, detail);
        (self.report)(reason);
    }

    fn cancel(&mut self) {
        match self.pending.take() {
            Some(Pending::Rvfc(id)) => {
                if let Ok(f) = js_sys::Reflect::get(&self.video, &"cancelVideoFrameCallback".into())
                    .and_then(|f| f.dyn_into::<js_sys::Function>())
                {
                    let _ = f.call1(&self.video, &JsValue::from_f64(id));
                }
            }
            Some(Pending::Raf(id)) => {
                if let Some(w) = web_sys::window() {
                    let _ = w.cancel_animation_frame(id);
                }
            }
            None => {}
        }
    }
}

type Shared = Rc<RefCell<Core>>;

/// Registers the next callback unless one is waiting.
fn schedule(core: &Shared) {
    let mut c = core.borrow_mut();
    if c.dead || c.pending.is_some() {
        return;
    }
    let Some(cb) = c
        .tick
        .as_ref()
        .map(|t| t.as_ref().unchecked_ref::<js_sys::Function>().clone())
    else {
        return;
    };
    if let Some(rvfc) = c.rvfc.clone() {
        if let Some(id) = rvfc.call1(&c.video, &cb).ok().and_then(|v| v.as_f64()) {
            c.pending = Some(Pending::Rvfc(id));
        }
    } else if let Some(id) = web_sys::window().and_then(|w| w.request_animation_frame(&cb).ok()) {
        c.pending = Some(Pending::Raf(id));
    }
}

/// The callback of the loop.
fn run(core: &Shared, media_s: Option<f64>) {
    let playing = {
        let mut c = core.borrow_mut();
        c.pending = None;
        if c.dead {
            return;
        }
        c.step(media_s);
        !c.dead && !c.video.paused() && !c.video.ended()
    };
    if playing {
        schedule(core);
    }
}

/// One frame now, from outside the loop; restarts it if the video plays.
fn kick(core: &Shared, measure: bool) {
    let playing = {
        let mut c = core.borrow_mut();
        if c.dead {
            return;
        }
        if measure {
            c.measure_due = true;
        }
        c.step(None);
        !c.dead && !c.video.paused() && !c.video.ended()
    };
    if playing {
        schedule(core);
    }
}

fn poke() {
    let core = CURRENT.with(|c| c.borrow().upgrade());
    if let Some(core) = core {
        kick(&core, true);
    }
}

/// The viewer's size choice; shows on the next frame without reloading.
pub fn set_size(size: SubtitleSize) {
    if SIZE.with(|s| s.replace(size)) != size {
        poke();
    }
}

/// Whether the control bar shows; the subtitles clear it while it does.
pub fn set_controls_visible(visible: bool) {
    if CONTROLS_VISIBLE.with(|c| c.replace(visible)) != visible {
        poke();
    }
}

type Listener = Closure<dyn FnMut(web_sys::Event)>;
type ObserverCb = Closure<dyn FnMut(js_sys::Array, JsValue)>;

/// The driver for one picked track. Dropping it stops the loop and removes
/// the canvas.
pub struct RustSubtitles {
    core: Shared,
    video: HtmlVideoElement,
    listeners: Vec<(&'static str, Listener)>,
    observer: Option<(web_sys::ResizeObserver, ObserverCb)>,
}

impl Drop for RustSubtitles {
    fn drop(&mut self) {
        for (name, l) in &self.listeners {
            let _ = self
                .video
                .remove_event_listener_with_callback(name, l.as_ref().unchecked_ref());
        }
        if let Some((o, _)) = &self.observer {
            o.disconnect();
        }
        let mut c = self.core.borrow_mut();
        c.dead = true;
        c.cancel();
        c.canvas.remove();
        let s = c.renderer.stats();
        web_sys::console::debug_1(
            &format!(
                "pharos: subtitles stats frames={} idle={} rastered={} cache_hits={}",
                s.frames, s.idle_frames, s.events_rastered, s.event_cache_hits
            )
            .into(),
        );
    }
}

async fn font() -> Result<Rc<Vec<u8>>, String> {
    if let Some(f) = FONT.with(|f| f.borrow().clone()) {
        return Ok(f);
    }
    let resp = Request::get(FONT_URL)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("status {}", resp.status()));
    }
    let bytes = Rc::new(resp.binary().await.map_err(|e| e.to_string())?);
    FONT.with(|f| *f.borrow_mut() = Some(Rc::clone(&bytes)));
    Ok(bytes)
}

/// Starts drawing the WebVTT at `vtt_url` over `<video id=video_id>`.
/// `report` hears of a failure after start (a GPU that does not come back).
/// An error means the `<track>` should show instead; it is logged here.
pub async fn rust_subtitles(
    video_id: &str,
    vtt_url: &str,
    report: Rc<dyn Fn(FallbackReason)>,
) -> Result<RustSubtitles, FallbackReason> {
    let lose = |reason: FallbackReason, detail: String| {
        fallback_log(reason, &detail);
        reason
    };
    let (font, text) = futures_util::future::join(font(), super::platform::get_text(vtt_url)).await;
    let font = font.map_err(|e| lose(FallbackReason::FontFetch, e))?;
    let text = text.map_err(|(s, r)| lose(FallbackReason::TrackFetch, format!("{s:?} {r}")))?;

    // From here the data is in hand; the first-subtitle budget covers
    // parsing, rastering setup and the first draw, not the network.
    let started = now();
    let video = super::platform::video_element(video_id)
        .and_then(|v| v.dyn_into::<HtmlVideoElement>().ok())
        .ok_or_else(|| lose(FallbackReason::NoVideo, format!("no <video id={video_id}>")))?;
    let frame_box = video
        .parent_element()
        .and_then(|p| p.dyn_into::<HtmlElement>().ok())
        .ok_or_else(|| lose(FallbackReason::NoVideo, "video has no parent".into()))?;
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| lose(FallbackReason::NoVideo, "no document".into()))?;
    if let Some(old) = document.get_element_by_id(CANVAS_ID) {
        old.remove();
    }
    let canvas = document
        .create_element("canvas")
        .ok()
        .and_then(|c| c.dyn_into::<HtmlCanvasElement>().ok())
        .ok_or_else(|| lose(FallbackReason::NoWebgl2, "create canvas".into()))?;
    canvas.set_id(CANVAS_ID);
    canvas.set_class_name(CANVAS_CLASS);
    let _ = canvas.set_attribute("aria-hidden", "true");
    // After the video, where Dioxus never diffs (see the watch view).
    frame_box
        .append_child(&canvas)
        .map_err(|e| lose(FallbackReason::NoWebgl2, format!("{e:?}")))?;
    let compositor = match render::open_subtitles(CANVAS_ID) {
        Ok(c) => c,
        Err(f) => {
            canvas.remove();
            let reason = match f {
                render::Fallback::NoWebgl2 | render::Fallback::NoCanvas => FallbackReason::NoWebgl2,
                _ => FallbackReason::Gpu,
            };
            return Err(lose(reason, format!("{f:?}")));
        }
    };
    let mut renderer = RustSubs::new();
    let setup = renderer
        .add_font(font.to_vec())
        .map_err(|e| (FallbackReason::Font, e.to_string()))
        .and_then(|()| {
            renderer
                .load(text)
                .map_err(|e| (FallbackReason::TrackFetch, e.to_string()))
        });
    if let Err((reason, detail)) = setup {
        canvas.remove();
        return Err(lose(reason, detail));
    }
    let size = SIZE.with(Cell::get);
    renderer.set_style(style_for(size));

    let rvfc = js_sys::Reflect::get(&video, &"requestVideoFrameCallback".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    let use_rvfc = rvfc.is_some();
    let core: Shared = Rc::new(RefCell::new(Core {
        video: video.clone(),
        canvas,
        frame_box: frame_box.clone(),
        renderer,
        compositor,
        viewport: Viewport::full(1, 1),
        size_applied: size,
        measure_due: true,
        pending: None,
        tick: None,
        rvfc,
        dead: false,
        report,
    }));
    let weak = Rc::downgrade(&core);
    let tick = {
        let weak = weak.clone();
        Closure::<dyn FnMut(JsValue, JsValue)>::new(move |_now, meta| {
            if let Some(core) = weak.upgrade() {
                // rvfc passes metadata with the exact media time; rAF passes
                // nothing and the element's clock is read instead.
                let media = js_sys::Reflect::get(&meta, &"mediaTime".into())
                    .ok()
                    .and_then(|v| v.as_f64());
                run(&core, media);
            }
        })
    };
    core.borrow_mut().tick = Some(tick);
    CURRENT.with(|c| *c.borrow_mut() = Rc::downgrade(&core));

    // Seeks, pauses and layout changes while no frame is coming.
    let mut events: Vec<(&'static str, bool)> = vec![
        ("play", false),
        ("playing", false),
        ("pause", false),
        ("seeking", false),
        ("seeked", false),
        ("loadedmetadata", true),
        ("resize", true),
    ];
    if !use_rvfc {
        events.push(("timeupdate", false));
    }
    let mut listeners = Vec::new();
    for (name, measure) in events {
        let weak = weak.clone();
        let l = Listener::new(move |_| {
            if let Some(core) = weak.upgrade() {
                kick(&core, measure);
            }
        });
        let _ = video.add_event_listener_with_callback(name, l.as_ref().unchecked_ref());
        listeners.push((name, l));
    }
    let observer = {
        let weak = weak.clone();
        let cb = Closure::<dyn FnMut(js_sys::Array, JsValue)>::new(move |_, _| {
            if let Some(core) = weak.upgrade() {
                kick(&core, true);
            }
        });
        web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref())
            .ok()
            .map(|o| {
                o.observe(&frame_box);
                (o, cb)
            })
    };
    let handle = RustSubtitles {
        core: Rc::clone(&core),
        video,
        listeners,
        observer,
    };

    // The first frame, then the budget.
    kick(&core, true);
    let spent = now() - started;
    if spent > f64::from(FIRST_SUBTITLE_BUDGET_MS) {
        return Err(lose(FallbackReason::Slow, format!("{spent:.0}ms")));
    }
    if core.borrow().dead {
        return Err(FallbackReason::Gpu);
    }
    Ok(handle)
}
