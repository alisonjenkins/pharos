//! Posters for the offline library, in the browser (spec 049, ADR-0149):
//! one copy of each noted item's picture in Cache Storage
//! `pharos-art-<user>`, under [`art_key`], which the service worker answers
//! a card's picture from at any width.
//!
//! Cost model, for TVs and headsets: one pass per notes pass that changed
//! something, fetching only pictures not stored yet; nothing else runs.

use std::collections::HashSet;

use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::offline_device::{art_key, ART_PREFIX, ART_WIDTH};
use super::platform::{info, warn};

fn describe(e: &JsValue) -> String {
    e.as_string()
        .or_else(|| js_sys::Reflect::get(e, &"message".into()).ok()?.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}

fn caches() -> Result<web_sys::CacheStorage, String> {
    web_sys::window()
        .ok_or("no window")?
        .caches()
        .map_err(|e| format!("Cache Storage is unavailable: {}", describe(&e)))
}

fn origin() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}

/// Stores the picture behind each of `images` (same-origin paths, as
/// `Item.image`) not stored yet, and drops the stored ones not listed.
pub async fn keep_art(user: &str, images: &[String]) {
    let began = super::platform::now_ms();
    let Ok(storage) = caches() else {
        return;
    };
    let name = format!("{ART_PREFIX}{user}");
    if images.is_empty() {
        // No cache at all, so the worker leaves every picture alone.
        if JsFuture::from(storage.has(&name))
            .await
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            let _ = JsFuture::from(storage.delete(&name)).await;
            notify_worker();
        }
        return;
    }
    let cache: web_sys::Cache = match JsFuture::from(storage.open(&name)).await {
        Ok(c) => match c.dyn_into() {
            Ok(c) => c,
            Err(_) => return,
        },
        Err(e) => {
            warn(&format!("pharos: offline art cache error={}", describe(&e)));
            return;
        }
    };
    let base = origin();
    let wanted: HashSet<String> = images
        .iter()
        .map(|i| match i.starts_with("http") {
            true => art_key(i),
            false => art_key(&format!("{base}{i}")),
        })
        .collect();
    let stored: HashSet<String> = match JsFuture::from(cache.keys()).await {
        Ok(keys) => js_sys::Array::from(&keys)
            .iter()
            .filter_map(|k| js_sys::Reflect::get(&k, &"url".into()).ok()?.as_string())
            .collect(),
        Err(_) => HashSet::new(),
    };
    let (mut fetched, mut failed, mut dropped) = (0u32, 0u32, 0u32);
    for key in stored.difference(&wanted) {
        if JsFuture::from(cache.delete_with_str(key)).await.is_ok() {
            dropped += 1;
        }
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    for key in wanted.difference(&stored) {
        let sep = if key.contains('?') { '&' } else { '?' };
        let url = format!("{key}{sep}fillWidth={ART_WIDTH}");
        let response = match JsFuture::from(window.fetch_with_str(&url)).await {
            Ok(r) => r.dyn_into::<web_sys::Response>().ok(),
            Err(_) => None,
        };
        match response.filter(web_sys::Response::ok) {
            Some(r) => match JsFuture::from(cache.put_with_str(key, &r)).await {
                Ok(_) => fetched += 1,
                Err(_) => failed += 1,
            },
            None => failed += 1,
        }
    }
    if fetched > 0 || dropped > 0 {
        notify_worker();
    }
    if fetched > 0 || failed > 0 || dropped > 0 {
        info(&format!(
            "pharos: offline art wanted={} fetched={fetched} failed={failed} dropped={dropped} ms={:.0}",
            wanted.len(),
            super::platform::now_ms() - began
        ));
    }
}

/// Deletes every stored poster, as signing out forgets what is kept.
pub async fn forget_art() {
    let Ok(storage) = caches() else {
        return;
    };
    let Ok(keys) = JsFuture::from(storage.keys()).await else {
        return;
    };
    for name in js_sys::Array::from(&keys)
        .iter()
        .filter_map(|k| k.as_string())
    {
        if name.starts_with(ART_PREFIX) {
            let _ = JsFuture::from(storage.delete(&name)).await;
        }
    }
    notify_worker();
}

/// Tells the service worker the set of poster caches changed.
fn notify_worker() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let navigator = window.navigator();
    let Ok(container) = js_sys::Reflect::get(&navigator, &"serviceWorker".into()) else {
        return;
    };
    let Ok(container) = container.dyn_into::<web_sys::ServiceWorkerContainer>() else {
        return;
    };
    let Some(worker) = container.controller() else {
        return;
    };
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &"kind".into(), &"pharos-sw".into());
    let _ = js_sys::Reflect::set(&message, &"event".into(), &"art-changed".into());
    if let Err(e) = worker.post_message(&message) {
        warn(&format!(
            "pharos: offline art worker note failed error={}",
            describe(&e)
        ));
    }
}
