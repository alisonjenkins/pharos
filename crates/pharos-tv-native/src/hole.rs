//! Video hole probe (spec 032 slice 0): can a layer under the UI show
//! through where Blitz draws nothing? A player would put MediaCodec's output
//! surface there and draw its controls above it, as Android's SurfaceView.
//!
//! `setprop debug.pharos.hole 1` (or 2) turns it on: the window format
//! becomes TRANSLUCENT (NativeActivity asks for an opaque RGB_565 window,
//! and SurfaceFlinger then ignores the buffer's alpha) and the renderer
//! clears to transparent. The layer under it, one colour in a 16:9 rect:
//!
//! - `1`: a SurfaceView added by the Kotlin side (`android/`).
//! - `3`: a child `android.view.SurfaceControl` at layer -1, made over JNI
//!   with the window's own SurfaceControl as parent (no Java written; the
//!   parent comes from the hidden `ViewRootImpl.getSurfaceControl`).
//! - `2`: a child `ASurfaceControl` at z -1. Android 12 refuses it on this
//!   window (a BLAST layer); kept to show that. ASurfaceControl is API 29
//!   (setCrop API 31) and dx links against API 24, so its calls are looked
//!   up at run time.

use android_activity::AndroidApp;
use std::ffi::{CStr, c_char, c_void};
use std::time::Duration;

#[manganis::ffi("android")]
extern "Kotlin" {
    pub type VideoHole;
}

/// The Kotlin class, by its binary name for `ClassLoader.loadClass`.
const VIDEO_HOLE_CLASS: &str = "com.alisonjenkins.pharos.tvnative.video.VideoHole";

fn mode() -> Option<String> {
    android_properties::getprop("debug.pharos.hole")
        .value()
        .filter(|v| matches!(v.as_str(), "1" | "2" | "3"))
}

pub fn enabled() -> bool {
    mode().is_some()
}

/// `android.graphics.PixelFormat.TRANSLUCENT`.
const PIXEL_FORMAT_TRANSLUCENT: i32 = -3;
/// `ASURFACE_TRANSACTION_VISIBILITY_SHOW`.
const VISIBILITY_SHOW: i8 = 1;
/// Below the parent window's own buffer.
const Z_BELOW_UI: i32 = -1;
/// `ADATASPACE_UNKNOWN`.
const DATASPACE_UNKNOWN: i32 = 0;
/// How long to wait for the window before giving up.
const WINDOW_WAIT: Duration = Duration::from_secs(10);
/// Where the "video" goes, in window pixels at 1080p: 1280x720 at (320, 80).
const VIDEO_RECT: ARect = ARect {
    left: 320,
    top: 80,
    right: 1600,
    bottom: 800,
};

#[repr(C)]
struct ARect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

type Txn = c_void;
type Sc = c_void;

struct Api {
    create_from_window: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut Sc,
    txn_create: unsafe extern "C" fn() -> *mut Txn,
    txn_delete: unsafe extern "C" fn(*mut Txn),
    txn_apply: unsafe extern "C" fn(*mut Txn),
    set_color: unsafe extern "C" fn(*mut Txn, *mut Sc, f32, f32, f32, f32, i32),
    set_z_order: unsafe extern "C" fn(*mut Txn, *mut Sc, i32),
    set_visibility: unsafe extern "C" fn(*mut Txn, *mut Sc, i8),
    set_crop: Option<unsafe extern "C" fn(*mut Txn, *mut Sc, *const ARect)>,
}

fn sym(lib: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: `lib` is a live dlopen handle and `name` is NUL-terminated.
    let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
    if p.is_null() {
        Err(format!("dlsym {name:?}: not found"))
    } else {
        Ok(p)
    }
}

impl Api {
    fn load() -> Result<Self, String> {
        // SAFETY: libandroid is always loaded in an app process; dlopen only
        // takes another reference to it.
        let lib = unsafe { libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW) };
        if lib.is_null() {
            return Err("dlopen libandroid.so failed".into());
        }
        // SAFETY: each symbol is transmuted to the signature its NDK header
        // declares.
        unsafe {
            Ok(Self {
                create_from_window: std::mem::transmute(sym(
                    lib,
                    c"ASurfaceControl_createFromWindow",
                )?),
                txn_create: std::mem::transmute(sym(lib, c"ASurfaceTransaction_create")?),
                txn_delete: std::mem::transmute(sym(lib, c"ASurfaceTransaction_delete")?),
                txn_apply: std::mem::transmute(sym(lib, c"ASurfaceTransaction_apply")?),
                set_color: std::mem::transmute(sym(lib, c"ASurfaceTransaction_setColor")?),
                set_z_order: std::mem::transmute(sym(lib, c"ASurfaceTransaction_setZOrder")?),
                set_visibility: std::mem::transmute(sym(
                    lib,
                    c"ASurfaceTransaction_setVisibility",
                )?),
                set_crop: sym(lib, c"ASurfaceTransaction_setCrop")
                    .ok()
                    .map(|p| std::mem::transmute(p)),
            })
        }
    }
}

/// Call before the event loop starts. Logs what it did or why it stopped.
pub fn start(app: &AndroidApp) {
    match translucent_window(app) {
        Ok(()) => log::info!("hole: window format TRANSLUCENT set"),
        Err(e) => {
            log::warn!("hole: window format: {e}");
            return;
        }
    }
    match mode().as_deref() {
        Some("1") => match surface_view(app) {
            Ok(()) => log::info!("hole: SurfaceView requested"),
            Err(e) => log::warn!("hole: SurfaceView: {e}"),
        },
        Some("3") => {
            let app = app.clone();
            let spawned = std::thread::Builder::new()
                .name("pharos-hole".into())
                .spawn(move || java_surface_control(&app));
            if let Err(e) = spawned {
                log::warn!("hole: thread: {e}");
            }
        }
        _ => surface_control(app),
    }
}

/// Mode 3: retries until the window's ViewRootImpl has a valid SurfaceControl.
fn java_surface_control(app: &AndroidApp) {
    // SAFETY: as in `translucent_window`.
    let vm = match unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) } {
        Ok(vm) => vm,
        Err(e) => return log::warn!("hole: vm: {e}"),
    };
    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(e) => return log::warn!("hole: attach: {e}"),
    };
    let activity = unsafe { jni::objects::JObject::from_raw(app.activity_as_ptr().cast()) };
    let started = std::time::Instant::now();
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match child_layer(&mut env, &activity) {
            Ok(true) => {
                log::info!(
                    "hole: java SurfaceControl child after {attempts} tries, {} ms",
                    started.elapsed().as_millis()
                );
                return;
            }
            Ok(false) => {}
            Err(e) => {
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_describe();
                    let _ = env.exception_clear();
                }
                return log::warn!("hole: java SurfaceControl: {e}");
            }
        }
        if started.elapsed() > WINDOW_WAIT {
            return log::warn!("hole: no valid window SurfaceControl after {attempts} tries");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `Ok(false)` while the window has no valid SurfaceControl yet.
fn child_layer(
    env: &mut jni::JNIEnv<'_>,
    activity: &jni::objects::JObject<'_>,
) -> jni::errors::Result<bool> {
    use jni::objects::{JObject, JValue};
    let window = env
        .call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?
        .l()?;
    let decor = env
        .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?
        .l()?;
    let root = env
        .call_method(
            &decor,
            "getViewRootImpl",
            "()Landroid/view/ViewRootImpl;",
            &[],
        )?
        .l()?;
    if root.is_null() {
        return Ok(false);
    }
    let parent = env
        .call_method(
            &root,
            "getSurfaceControl",
            "()Landroid/view/SurfaceControl;",
            &[],
        )?
        .l()?;
    if parent.is_null() || !env.call_method(&parent, "isValid", "()Z", &[])?.z()? {
        return Ok(false);
    }
    // A server playback fills the window: the decoder's buffer, at the
    // source's own size (4K stays 4K), is scaled to it by the compositor.
    let rect = if crate::video::full_window() {
        let w = env.call_method(&decor, "getWidth", "()I", &[])?.i()?;
        let h = env.call_method(&decor, "getHeight", "()I", &[])?.i()?;
        log::info!("hole: full window {w}x{h}");
        ARect {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        }
    } else {
        VIDEO_RECT
    };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    let builder = env.new_object("android/view/SurfaceControl$Builder", "()V", &[])?;
    let name = env.new_string("pharos-video-hole")?;
    const B: &str = "Landroid/view/SurfaceControl$Builder;";
    env.call_method(
        &builder,
        "setName",
        format!("(Ljava/lang/String;){B}"),
        &[JValue::Object(&name)],
    )?;
    env.call_method(
        &builder,
        "setBufferSize",
        format!("(II){B}"),
        &[JValue::Int(width), JValue::Int(height)],
    )?;
    env.call_method(
        &builder,
        "setParent",
        format!("(Landroid/view/SurfaceControl;){B}"),
        &[JValue::Object(&parent)],
    )?;
    let child = env
        .call_method(&builder, "build", "()Landroid/view/SurfaceControl;", &[])?
        .l()?;

    let dst = env.new_object(
        "android/graphics/Rect",
        "(IIII)V",
        &[
            JValue::Int(rect.left),
            JValue::Int(rect.top),
            JValue::Int(rect.right),
            JValue::Int(rect.bottom),
        ],
    )?;
    let txn = env.new_object("android/view/SurfaceControl$Transaction", "()V", &[])?;
    const T: &str = "Landroid/view/SurfaceControl$Transaction;";
    const SC: &str = "Landroid/view/SurfaceControl;";
    env.call_method(
        &txn,
        "setLayer",
        format!("({SC}I){T}"),
        &[JValue::Object(&child), JValue::Int(Z_BELOW_UI)],
    )?;
    env.call_method(
        &txn,
        "setGeometry",
        format!("({SC}Landroid/graphics/Rect;Landroid/graphics/Rect;I){T}"),
        &[
            JValue::Object(&child),
            JValue::Object(&JObject::null()),
            JValue::Object(&dst),
            JValue::Int(0),
        ],
    )?;
    env.call_method(
        &txn,
        "setVisibility",
        format!("({SC}Z){T}"),
        &[JValue::Object(&child), JValue::Bool(1)],
    )?;
    env.call_method(&txn, "apply", "()V", &[])?;

    // What a decoder would render into: MediaCodec takes this Surface.
    let surface = env.new_object(
        "android/view/Surface",
        format!("({SC})V"),
        &[JValue::Object(&child)],
    )?;
    if let Some(path) = crate::video::path() {
        // A canvas fill would connect the Surface as a CPU producer and the
        // decoder could not connect after it, so a video gets no fill.
        // SAFETY: `env` and `surface` are live for this call; the window
        // takes its own reference to the Surface's producer.
        match unsafe {
            ndk::native_window::NativeWindow::from_surface(
                env.get_raw().cast(),
                surface.as_raw().cast(),
            )
        } {
            Some(window) => crate::video::play(window, path),
            None => log::warn!("hole: ANativeWindow_fromSurface returned null"),
        }
    } else {
        let canvas = env
            .call_method(
                &surface,
                "lockCanvas",
                "(Landroid/graphics/Rect;)Landroid/graphics/Canvas;",
                &[JValue::Object(&JObject::null())],
            )?
            .l()?;
        env.call_method(&canvas, "drawColor", "(I)V", &[JValue::Int(GREEN)])?;
        env.call_method(
            &surface,
            "unlockCanvasAndPost",
            "(Landroid/graphics/Canvas;)V",
            &[JValue::Object(&canvas)],
        )?;
    }
    // The layer is released when its SurfaceControl is collected; the probe
    // keeps both for the life of the process.
    std::mem::forget(env.new_global_ref(&child)?);
    std::mem::forget(env.new_global_ref(&surface)?);
    Ok(true)
}

/// ARGB green, so a screencap tells mode 3 from mode 1's red.
const GREEN: i32 = 0xFF19_C819_u32 as i32;

/// `new VideoHole(activity).show(..)`. The class is loaded through the
/// activity's class loader: `FindClass` on this native thread would search
/// only the system one.
fn surface_view(app: &AndroidApp) -> Result<(), String> {
    use jni::objects::{JClass, JObject, JValue};
    // SAFETY: as in `translucent_window`.
    let vm =
        unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }.map_err(|e| format!("vm: {e}"))?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach: {e}"))?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let result = (|| -> jni::errors::Result<()> {
        let loader = env
            .call_method(
                &activity,
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[],
            )?
            .l()?;
        let name = env.new_string(VIDEO_HOLE_CLASS)?;
        let class = env
            .call_method(
                &loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[JValue::Object(&name)],
            )?
            .l()?;
        let hole = env.new_object(
            JClass::from(class),
            "(Landroid/app/Activity;)V",
            &[JValue::Object(&activity)],
        )?;
        env.call_method(
            &hole,
            "show",
            "(IIII)V",
            &[
                JValue::Int(VIDEO_RECT.left),
                JValue::Int(VIDEO_RECT.top),
                JValue::Int(VIDEO_RECT.right - VIDEO_RECT.left),
                JValue::Int(VIDEO_RECT.bottom - VIDEO_RECT.top),
            ],
        )?;
        Ok(())
    })();
    if result.is_err() && env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result.map_err(|e| format!("{e}"))
}

fn surface_control(app: &AndroidApp) {
    let api = match Api::load() {
        Ok(api) => api,
        Err(e) => {
            log::warn!("hole: {e}");
            return;
        }
    };
    let app = app.clone();
    let spawned = std::thread::Builder::new().name("pharos-hole".into()).spawn(move || {
        let started = std::time::Instant::now();
        let mut attempts = 0u32;
        let (window, sc) = loop {
            if let Some(w) = app.native_window() {
                attempts += 1;
                // SAFETY: `w` holds a reference to the ANativeWindow for the
                // call; the child layer keeps its own reference to the parent.
                let sc = unsafe {
                    (api.create_from_window)(w.ptr().as_ptr().cast(), c"pharos-video-hole".as_ptr())
                };
                if !sc.is_null() {
                    break (w, sc);
                }
            }
            if started.elapsed() > WINDOW_WAIT {
                log::warn!("hole: no child layer after {WINDOW_WAIT:?}, {attempts} createFromWindow calls");
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        log::info!("hole: child layer after {attempts} createFromWindow calls, {} ms", started.elapsed().as_millis());
        // SAFETY: `txn` and `sc` are live handles from the calls above.
        unsafe {
            let txn = (api.txn_create)();
            (api.set_color)(txn, sc, 0.9, 0.1, 0.1, 1.0, DATASPACE_UNKNOWN);
            (api.set_z_order)(txn, sc, Z_BELOW_UI);
            (api.set_visibility)(txn, sc, VISIBILITY_SHOW);
            if let Some(crop) = api.set_crop {
                crop(txn, sc, &VIDEO_RECT);
            }
            (api.txn_apply)(txn);
            (api.txn_delete)(txn);
        }
        log::info!(
            "hole: colour layer z={Z_BELOW_UI} crop={} window={}x{} format={:?}",
            api.set_crop.is_some(),
            window.width(),
            window.height(),
            window.format()
        );
        // The layer lives as long as the process; the probe never removes it.
    });
    if let Err(e) = spawned {
        log::warn!("hole: thread: {e}");
    }
}

/// `activity.getWindow().setFormat(TRANSLUCENT)`. NativeActivity exposes no
/// ANativeActivity pointer through android-activity, so this goes by JNI.
fn translucent_window(app: &AndroidApp) -> Result<(), String> {
    use jni::objects::{JObject, JValue};
    // SAFETY: both pointers come from android-activity and stay valid while
    // `app` lives; the activity is a global reference, never deleted here.
    let vm =
        unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }.map_err(|e| format!("vm: {e}"))?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach: {e}"))?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let result = env
        .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])
        .and_then(|w| w.l())
        .and_then(|window| {
            env.call_method(
                &window,
                "setFormat",
                "(I)V",
                &[JValue::Int(PIXEL_FORMAT_TRANSLUCENT)],
            )
        });
    if result.is_err() && env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result.map(|_| ()).map_err(|e| format!("setFormat: {e}"))
}
