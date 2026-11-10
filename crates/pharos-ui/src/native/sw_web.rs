//! Registers the shell's service worker (spec 036 FR-003, ADR-0110) and logs
//! what it does. Browser only; the TV app and host builds never see it.
//!
//! Cost model, for TVs and headsets: one registration call at start-up and
//! one message listener. The worker itself is idle until the browser wakes
//! it for a fetch.
//!
//! The adapter is the only code that knows the worker exists, so turning it
//! off is [`register`] returning early: set `localStorage` `pharos.service_worker`
//! to `off` and the next start unregisters the worker instead.

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

/// The worker script. Its scope is its directory, `/ui/`, which is all of the
/// app, so no `Service-Worker-Allowed` header is needed.
const SCRIPT: &str = "/ui/sw.js";
const OFF_KEY: &str = "pharos.service_worker";
const OFF_VALUE: &str = "off";
/// Marks the messages the worker posts and the one the page sends.
const MESSAGE_KIND: &str = "pharos-sw";

/// Starts registering (or, when switched off, unregistering) the worker. A
/// failure is logged and the app carries on: plain http off localhost has no
/// service workers, and the shell works without one.
pub fn register() {
    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = start().await {
            log_warn(&format!("service worker unavailable: {}", describe(&e)));
        }
    });
}

async fn start() -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let navigator = window.navigator();
    let container = js_sys::Reflect::get(&navigator, &"serviceWorker".into())?;
    if container.is_undefined() {
        log_info("service worker unsupported here (needs https or localhost)");
        return Ok(());
    }
    let container: web_sys::ServiceWorkerContainer = container.dyn_into()?;

    if switched_off(&window) {
        for registration in
            js_sys::Array::from(&JsFuture::from(container.get_registrations()).await?)
                .iter()
                .filter_map(|r| r.dyn_into::<web_sys::ServiceWorkerRegistration>().ok())
        {
            JsFuture::from(registration.unregister()?).await?;
            log_info("service worker switched off, unregistered");
        }
        return Ok(());
    }

    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(|event: web_sys::MessageEvent| {
            let data = event.data();
            if text(&data, "kind").as_deref() != Some(MESSAGE_KIND) {
                return;
            }
            log_info(&format!(
                "service worker {} version {}",
                text(&data, "event").unwrap_or_default(),
                text(&data, "version").unwrap_or_default()
            ));
        });
    container.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref())?;
    // Lives as long as the page; there is one.
    on_message.forget();

    let registration: web_sys::ServiceWorkerRegistration =
        JsFuture::from(container.register(SCRIPT))
            .await?
            .dyn_into()?;
    log_info(&format!(
        "service worker registered, scope {}",
        registration.scope()
    ));

    let ready: web_sys::ServiceWorkerRegistration =
        JsFuture::from(container.ready()?).await?.dyn_into()?;
    if let Some(active) = ready.active() {
        let hello = js_sys::Object::new();
        js_sys::Reflect::set(&hello, &"kind".into(), &MESSAGE_KIND.into())?;
        js_sys::Reflect::set(&hello, &"event".into(), &"hello".into())?;
        active.post_message(&hello)?;
    }
    Ok(())
}

fn switched_off(window: &web_sys::Window) -> bool {
    window
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(OFF_KEY).ok().flatten())
        .is_some_and(|v| v == OFF_VALUE)
}

fn text(data: &JsValue, field: &str) -> Option<String> {
    js_sys::Reflect::get(data, &field.into()).ok()?.as_string()
}

fn describe(e: &JsValue) -> String {
    e.as_string()
        .or_else(|| {
            e.dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .unwrap_or_else(|| format!("{e:?}"))
}

fn log_info(msg: &str) {
    web_sys::console::info_1(&format!("pharos: {msg}").into());
}

fn log_warn(msg: &str) {
    web_sys::console::warn_1(&format!("pharos: {msg}").into());
}
