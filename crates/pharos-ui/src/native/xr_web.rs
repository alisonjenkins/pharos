//! Watching a packed 3D version in stereo on a headset (spec 022 US3,
//! ADR 0088). The compositor draws the `<video>` into each eye through a
//! WebXR media quad layer, so no frame is copied by the page. WebXR is not in
//! web-sys's stable bindings, so it is reached through `Reflect` here rather
//! than `web_sys_unstable_apis`, which would rebuild every crate.

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Array, Function, Object, Promise, Reflect};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::platform::{info, warn};
use super::stereo::VR_SCREEN_DISTANCE_M;

type Listener = Closure<dyn FnMut(JsValue)>;

/// The session running, with the callbacks it holds, so they live as long
/// as it does.
struct Live {
    session: JsValue,
    _listeners: Vec<Listener>,
    _frame: Rc<RefCell<Option<Listener>>>,
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
/// screen, each eye its half of the frame by `layout`; the controller's
/// select calls `on_select`. Must run inside the click that asked for it: the
/// session request needs that user activation, so nothing is awaited first.
pub fn enter_vr(
    video_id: &str,
    layout: &'static str,
    (width, height): (f64, f64),
    on_select: Rc<dyn Fn()>,
) {
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
        "requiredFeatures",
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
        if let Err(e) = show(&session, &video, layout, (width, height), &frame).await {
            warn(&format!(
                "pharos: xr session failed reason=layer error={e:?}"
            ));
            let _ = call(&session, "end", &[]);
        }
    });
}

/// Listens to `session` and keeps it as the one running, ending any other.
/// Returns the slot its frame loop goes in.
fn hold(session: &JsValue, on_select: Rc<dyn Fn()>) -> Rc<RefCell<Option<Listener>>> {
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
    frame: &Rc<RefCell<Option<Listener>>>,
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
    *frame.borrow_mut() = Some(Closure::new(move |_| {
        if let Some(f) = next.upgrade() {
            if let Some(cb) = f.borrow().as_ref() {
                let _ = call(&s, "requestAnimationFrame", &[cb.as_ref().clone()]);
            }
        }
    }));
    if let Some(cb) = frame.borrow().as_ref() {
        call(session, "requestAnimationFrame", &[cb.as_ref().clone()])?;
    }
    info(&format!(
        "pharos: xr session started layout={layout} path=layers"
    ));
    Ok(())
}

/// Leaves VR, if a session is running: the player is going away.
pub fn exit_vr() {
    let session = LIVE.with(|l| l.borrow().as_ref().map(|live| live.session.clone()));
    if let Some(session) = session {
        let _ = call(&session, "end", &[]);
    }
}
