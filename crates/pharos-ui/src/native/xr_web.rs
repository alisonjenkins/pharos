//! Watching a packed 3D version in stereo on a headset (spec 022 US3,
//! ADR 0088). Where the browser has WebXR layers, the compositor draws the
//! `<video>` into each eye through a media quad layer, so no frame is copied
//! by the page. Elsewhere the page draws it: one quad per eye through WebGL2
//! (`render::webgl_xr`), the frame uploaded once per video frame. WebXR is not in
//! web-sys's stable bindings, so it is reached through `Reflect` here rather
//! than `web_sys_unstable_apis`, which would rebuild every crate.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::{Array, Function, Object, Promise, Reflect};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

use pharos_client::api::Stereo;
use web_sys::{HtmlVideoElement, WebGl2RenderingContext as Gl};

use super::platform::{info, warn};
use super::stereo::{self, Eye, XrPath, VR_SCREEN_DISTANCE_M};
use crate::render::stereo_quad::{eye_uniform, screen_model};
use crate::render::webgl_xr::EyeQuads;

type Listener = Closure<dyn FnMut(JsValue)>;
/// A session's frame callback: `(time, XRFrame)`.
type FrameCallback = Closure<dyn FnMut(JsValue, JsValue)>;
type FrameSlot = Rc<RefCell<Option<FrameCallback>>>;

/// The session running, with the callbacks it holds, so they live as long
/// as it does.
struct Live {
    session: JsValue,
    _listeners: Vec<Listener>,
    _frame: FrameSlot,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}

fn get(o: &JsValue, key: &str) -> JsValue {
    Reflect::get(o, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

fn set(o: &Object, key: &str, value: &JsValue) {
    let _ = Reflect::set(o, &JsValue::from_str(key), value);
}

fn call(o: &JsValue, key: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let f: Function = get(o, key)
        .dyn_into()
        .map_err(|_| JsValue::from_str(&format!("no {key}()")))?;
    f.apply(o, &args.iter().collect::<Array>())
}

async fn wait(p: JsValue) -> Result<JsValue, JsValue> {
    JsFuture::from(p.dyn_into::<Promise>()?).await
}

fn xr() -> Option<JsValue> {
    let xr = get(&web_sys::window()?.navigator().into(), "xr");
    (!xr.is_undefined() && !xr.is_null()).then_some(xr)
}

/// Whether this browser can enter an immersive VR session (V923). Every
/// "no" is logged with its reason, so a missing button can be explained.
pub async fn vr_supported() -> bool {
    let Some(xr) = xr() else {
        info("pharos: xr supported=false reason=no-webxr");
        return false;
    };
    let answer = match call(
        &xr,
        "isSessionSupported",
        &[JsValue::from_str("immersive-vr")],
    ) {
        Ok(p) => wait(p).await,
        Err(e) => Err(e),
    };
    match answer.map(|v| v.as_bool()) {
        Ok(Some(supported)) => {
            info(&format!("pharos: xr supported={supported}"));
            supported
        }
        Ok(None) => {
            warn("pharos: xr supported=false reason=not-a-bool");
            false
        }
        Err(e) => {
            warn(&format!(
                "pharos: xr supported=false reason=rejected error={e:?}"
            ));
            false
        }
    }
}

/// Enters VR showing the `<video>` `video_id` as a `width` x `height` metre
/// screen, each eye its half of the frame packed as `packing`; the controller's
/// select calls `on_select`. Must run inside the click that asked for it: the
/// session request needs that user activation, so nothing is awaited first.
pub fn enter_vr(
    video_id: &str,
    packing: Stereo,
    (width, height): (f64, f64),
    on_select: Rc<dyn Fn()>,
) {
    let Some(layout) = stereo::xr_layout(Some(packing)) else {
        warn("pharos: xr session failed reason=not-packed");
        return;
    };
    let Some(xr) = xr() else {
        warn("pharos: xr session failed reason=no-webxr");
        return;
    };
    let Some(video) = super::platform::video_element(video_id) else {
        warn(&format!(
            "pharos: xr session failed reason=no-video id={video_id}"
        ));
        return;
    };
    let init = Object::new();
    set(
        &init,
        "optionalFeatures",
        &Array::of1(&JsValue::from_str("layers")),
    );
    let request = call(
        &xr,
        "requestSession",
        &[JsValue::from_str("immersive-vr"), init.into()],
    );
    wasm_bindgen_futures::spawn_local(async move {
        let started = match request {
            Ok(p) => wait(p).await,
            Err(e) => Err(e),
        };
        let session = match started {
            Ok(s) => s,
            Err(e) => {
                warn(&format!(
                    "pharos: xr session failed reason=request error={e:?}"
                ));
                return;
            }
        };
        // Listen before anything is awaited, so an end during setup is seen.
        let frame = hold(&session, on_select);
        let (granted, binding) = layer_support(&session);
        let path = XrPath::choose(granted, binding);
        let facts = format!(
            "packing={} layout={layout} path={} layers-granted={granted} media-binding={binding}",
            stereo::packing_attr(packing),
            path.as_str()
        );
        let shown = match path {
            XrPath::Layers => show(&session, &video, layout, (width, height), &frame).await,
            XrPath::Webgl => show_gl(&session, &video, packing, (width, height), &frame).await,
        };
        match shown {
            Ok(()) => info(&format!("pharos: xr session started {facts}")),
            Err(e) => {
                warn(&format!(
                    "pharos: xr session failed reason=show-failed {facts} error={e:?}"
                ));
                let _ = call(&session, "end", &[]);
            }
        }
    });
}

/// Listens to `session` and keeps it as the one running, ending any other.
/// Returns the slot its frame loop goes in.
fn hold(session: &JsValue, on_select: Rc<dyn Fn()>) -> FrameSlot {
    let mut listeners = Vec::new();
    let mut on = |kind: &str, f: Listener| {
        let _ = call(
            session,
            "addEventListener",
            &[JsValue::from_str(kind), f.as_ref().clone()],
        );
        listeners.push(f);
    };
    on("select", Closure::new(move |_| on_select()));
    let squeezed = session.clone();
    on(
        "squeeze",
        Closure::new(move |_| {
            let _ = call(&squeezed, "end", &[]);
        }),
    );
    let this = session.clone();
    on(
        "end",
        Closure::new(move |_| {
            info("pharos: xr session ended");
            // Only this session's own state; dropped later, not from inside
            // one of its own callbacks.
            let live = LIVE.with(|l| {
                let mut l = l.borrow_mut();
                l.as_ref()
                    .is_some_and(|live| live.session == this)
                    .then(|| l.take())
                    .flatten()
            });
            if let Some(live) = live {
                wasm_bindgen_futures::spawn_local(async move { drop(live) });
            }
        }),
    );
    let frame = Rc::new(RefCell::new(None));
    let before = LIVE.with(|l| {
        l.borrow_mut().replace(Live {
            session: session.clone(),
            _listeners: listeners,
            _frame: Rc::clone(&frame),
        })
    });
    if let Some(before) = before {
        let _ = call(&before.session, "end", &[]);
        wasm_bindgen_futures::spawn_local(async move { drop(before) });
    }
    frame
}

/// Puts the `<video>` on a quad layer in front of the viewer.
async fn show(
    session: &JsValue,
    video: &web_sys::HtmlMediaElement,
    layout: &'static str,
    (width, height): (f64, f64),
    frame: &FrameSlot,
) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let space = wait(call(
        session,
        "requestReferenceSpace",
        &[JsValue::from_str("local")],
    )?)
    .await?;
    let binding_class: Function = get(&window, "XRMediaBinding")
        .dyn_into()
        .map_err(|_| JsValue::from_str("no XRMediaBinding"))?;
    let binding = Reflect::construct(&binding_class, &Array::of1(session))?;
    let rigid_class: Function = get(&window, "XRRigidTransform")
        .dyn_into()
        .map_err(|_| JsValue::from_str("no XRRigidTransform"))?;
    let at = Object::new();
    set(&at, "x", &0.0.into());
    set(&at, "y", &0.0.into());
    set(&at, "z", &(-VR_SCREEN_DISTANCE_M).into());
    let transform = Reflect::construct(&rigid_class, &Array::of1(&at))?;
    let quad = Object::new();
    set(&quad, "space", &space);
    set(&quad, "layout", &JsValue::from_str(layout));
    set(&quad, "transform", &transform);
    set(&quad, "width", &width.into());
    set(&quad, "height", &height.into());
    let layer = call(&binding, "createQuadLayer", &[video.into(), quad.into()])?;
    let state = Object::new();
    set(&state, "layers", &Array::of1(&layer));
    call(session, "updateRenderState", &[state.into()])?;

    // The media layer draws itself, but a session with no frame callback is
    // treated as hung by some runtimes, so one runs, drawing nothing.
    let (next, s) = (Rc::downgrade(frame), session.clone());
    *frame.borrow_mut() = Some(Closure::new(move |_, _| {
        if let Some(f) = next.upgrade() {
            if let Some(cb) = f.borrow().as_ref() {
                let _ = call(&s, "requestAnimationFrame", &[cb.as_ref().clone()]);
            }
        }
    }));
    if let Some(cb) = frame.borrow().as_ref() {
        call(session, "requestAnimationFrame", &[cb.as_ref().clone()])?;
    }
    Ok(())
}

/// Whether `session` was granted WebXR layers, and whether the browser has
/// `XRMediaBinding`.
fn layer_support(session: &JsValue) -> (bool, bool) {
    let enabled = get(session, "enabledFeatures");
    let granted = Array::is_array(&enabled)
        && Array::from(&enabled).includes(&JsValue::from_str("layers"), 0);
    let binding = web_sys::window().is_some_and(|w| get(&w, "XRMediaBinding").is_function());
    (granted, binding)
}

/// Marks each new frame of a `<video>` through `requestVideoFrameCallback`,
/// so the WebGL path uploads once per video frame, not once per display
/// frame: `currentTime` moves on between display frames while playing.
struct VideoFrames {
    video: HtmlVideoElement,
    fresh: Rc<Cell<bool>>,
    seen: Rc<Cell<bool>>,
    handle: Rc<Cell<f64>>,
    callback: FrameSlot,
}

impl VideoFrames {
    fn watch(video: &HtmlVideoElement) -> VideoFrames {
        let frames = VideoFrames {
            video: video.clone(),
            fresh: Rc::new(Cell::new(true)),
            seen: Rc::new(Cell::new(false)),
            handle: Rc::new(Cell::new(0.0)),
            callback: Rc::new(RefCell::new(None)),
        };
        if !get(video, "requestVideoFrameCallback").is_function() {
            info("pharos: xr video frames reason=no-request-video-frame-callback");
            return frames;
        }
        let (fresh, seen, handle) = (
            frames.fresh.clone(),
            frames.seen.clone(),
            frames.handle.clone(),
        );
        let (next, v) = (Rc::downgrade(&frames.callback), video.clone());
        *frames.callback.borrow_mut() = Some(Closure::new(move |_now, _meta| {
            fresh.set(true);
            seen.set(true);
            if let Some(slot) = next.upgrade() {
                if let Some(cb) = slot.borrow().as_ref() {
                    if let Ok(id) = call(&v, "requestVideoFrameCallback", &[cb.as_ref().clone()]) {
                        handle.set(id.as_f64().unwrap_or(0.0));
                    }
                }
            }
        }));
        if let Some(cb) = frames.callback.borrow().as_ref() {
            if let Ok(id) = call(video, "requestVideoFrameCallback", &[cb.as_ref().clone()]) {
                frames.handle.set(id.as_f64().unwrap_or(0.0));
            }
        }
        frames
    }

    /// Whether the video may show a new frame: one arrived since the last
    /// ask, or the browser has reported none (no callback, or one that does
    /// not run during the session), so every display frame may be new.
    fn take(&self) -> bool {
        self.fresh.replace(false) || !self.seen.get()
    }
}

impl Drop for VideoFrames {
    fn drop(&mut self) {
        let handle = JsValue::from_f64(self.handle.get());
        let _ = call(&self.video, "cancelVideoFrameCallback", &[handle]);
    }
}

/// Draws the `<video>` into each eye through WebGL2: the fallback for a
/// browser without media layers (ADR 0088).
async fn show_gl(
    session: &JsValue,
    video: &web_sys::HtmlMediaElement,
    packing: Stereo,
    (width, height): (f64, f64),
    frame: &FrameSlot,
) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let video: HtmlVideoElement = video.clone().dyn_into()?;
    let space = wait(call(
        session,
        "requestReferenceSpace",
        &[JsValue::from_str("local")],
    )?)
    .await?;
    let canvas: web_sys::HtmlCanvasElement = window
        .document()
        .ok_or("no document")?
        .create_element("canvas")?
        .dyn_into()?;
    let options = Object::new();
    set(&options, "xrCompatible", &JsValue::TRUE);
    let gl: Gl = canvas
        .get_context_with_context_options("webgl2", &options)?
        .ok_or("no webgl2")?
        .dyn_into()?;
    // xrCompatible at creation is a hint; XRWebGLLayer throws unless the
    // context was made compatible with the session's device.
    if get(&gl, "makeXRCompatible").is_function() {
        wait(call(&gl, "makeXRCompatible", &[])?).await?;
    }
    let quads = EyeQuads::new(gl.clone()).map_err(|e| JsValue::from_str(&format!("{e:?}")))?;
    let layer_class: Function = get(&window, "XRWebGLLayer")
        .dyn_into()
        .map_err(|_| JsValue::from_str("no XRWebGLLayer"))?;
    let base = Reflect::construct(
        &layer_class,
        &Array::of2(session, &JsValue::from(gl.clone())),
    )?;
    let state = Object::new();
    set(&state, "baseLayer", &base);
    call(session, "updateRenderState", &[state.into()])?;

    let model = screen_model(width, height);
    let eyes = [
        eye_uniform(stereo::eye_rect(Some(packing), Eye::Left)),
        eye_uniform(stereo::eye_rect(Some(packing), Eye::Right)),
    ];
    let mut quads = quads;
    let frames = VideoFrames::watch(&video);
    let mut upload_failed = false;
    let (next, s) = (Rc::downgrade(frame), session.clone());
    *frame.borrow_mut() = Some(Closure::new(move |_time, xr_frame: JsValue| {
        if let Some(f) = next.upgrade() {
            if let Some(cb) = f.borrow().as_ref() {
                let _ = call(&s, "requestAnimationFrame", &[cb.as_ref().clone()]);
            }
        }
        if frames.take() {
            if let Err(e) = quads.upload(&video) {
                if !upload_failed {
                    warn(&format!("pharos: xr frame upload failed error={e}"));
                    upload_failed = true;
                }
            }
        }
        let framebuffer = get(&base, "framebuffer");
        gl.bind_framebuffer(
            Gl::FRAMEBUFFER,
            framebuffer.dyn_ref::<web_sys::WebGlFramebuffer>(),
        );
        // Black, not last frame's picture, while tracking is lost.
        quads.clear();
        let Ok(pose) = call(&xr_frame, "getViewerPose", std::slice::from_ref(&space)) else {
            return;
        };
        if pose.is_null() || pose.is_undefined() {
            return;
        }
        for view in Array::from(&get(&pose, "views")).iter() {
            let Ok(vp) = call(&base, "getViewport", std::slice::from_ref(&view)) else {
                continue;
            };
            let n = |k: &str| get(&vp, k).as_f64().unwrap_or(0.0) as i32;
            let matrix = |o: &JsValue| js_sys::Float32Array::from(o.clone()).to_vec();
            let projection = matrix(&get(&view, "projectionMatrix"));
            let inverse = matrix(&get(&get(&get(&view, "transform"), "inverse"), "matrix"));
            let eye = match get(&view, "eye").as_string().as_deref() {
                Some("right") => eyes[1],
                _ => eyes[0],
            };
            quads.draw(
                (n("x"), n("y"), n("width"), n("height")),
                &projection,
                &inverse,
                &model,
                eye,
            );
        }
    }));
    if let Some(cb) = frame.borrow().as_ref() {
        call(session, "requestAnimationFrame", &[cb.as_ref().clone()])?;
    }
    Ok(())
}

/// Leaves VR, if a session is running: the player is going away.
pub fn exit_vr() {
    let session = LIVE.with(|l| l.borrow().as_ref().map(|live| live.session.clone()));
    if let Some(session) = session {
        let _ = call(&session, "end", &[]);
    }
}
