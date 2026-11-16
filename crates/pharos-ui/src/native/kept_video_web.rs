//! Downloaded video in the browser (spec 048, ADR 0143): the player's own
//! HLS stream stored in Cache Storage under `pharos-videos-<user>`, one
//! small `localStorage` value (`pharos.kept_videos`) recording what is kept.
//!
//! Every key is the absolute URL the player requests, query included, so
//! the service worker's `caches.match(request.url)` finds it with no
//! player change. Bytes go from the network into the cache without being
//! copied through wasm memory; only the small playlists are read as text.
//!
//! Cost model, for TVs and headsets: registration reads one `localStorage`
//! value and posts nothing unless a download was left half done.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::Arc;

use futures_util::stream::{self, StreamExt as _};
use pharos_hls::offline::{estimate_bytes, plan_master, plan_media};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::kept_video::{
    absolute_url, cache_name, check_not_kept, find, fits, insert_front, interrupted, is_retryable,
    missing_files, needs_admission, new_record, progress_due, put_is_retryable, reserved_by,
    retry_delay_secs, stale_files, still_needed, take, url_path, used_by, KeepVideoRefusal,
    KeepVideoRequest, KeptState, KeptVideo, VideoStore, DEFAULT_CAP_BYTES, RECORDS_KEY,
};
use super::platform::{self, info, warn};

const SW_KIND: &str = "pharos-sw";
const CONCURRENCY: usize = 2;

thread_local! {
    static LIST: RefCell<Option<Vec<KeptVideo>>> = const { RefCell::new(None) };
    static FREE: Cell<u64> = const { Cell::new(0) };
    static PERSIST_ASKED: Cell<bool> = const { Cell::new(false) };
    static LAST_SAVE_MS: Cell<f64> = const { Cell::new(0.0) };
}

fn describe(e: &JsValue) -> String {
    e.as_string()
        .or_else(|| {
            e.dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .unwrap_or_else(|| format!("{e:?}"))
}

fn local_storage() -> Result<web_sys::Storage, String> {
    web_sys::window()
        .ok_or("no window")?
        .local_storage()
        .map_err(|e| describe(&e))?
        .ok_or_else(|| "localStorage is unavailable".to_owned())
}

fn load() -> Vec<KeptVideo> {
    let text = match local_storage().and_then(|s| s.get_item(RECORDS_KEY).map_err(|e| describe(&e)))
    {
        Ok(Some(text)) => text,
        Ok(None) => return Vec::new(),
        Err(e) => {
            warn(&format!("pharos: video record unreadable error={e}"));
            return Vec::new();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        warn(&format!("pharos: video record damaged error={e}"));
        Vec::new()
    })
}

fn with_list<R>(f: impl FnOnce(&mut Vec<KeptVideo>) -> R) -> R {
    LIST.with(|cell| {
        let mut slot = cell.borrow_mut();
        f(slot.get_or_insert_with(load))
    })
}

fn save() -> Result<(), String> {
    let text = with_list(|list| serde_json::to_string(list));
    let result = text.map_err(|e| e.to_string()).and_then(|t| {
        local_storage().and_then(|s| s.set_item(RECORDS_KEY, &t).map_err(|e| describe(&e)))
    });
    match &result {
        Ok(()) => LAST_SAVE_MS.with(|c| c.set(js_sys::Date::now())),
        Err(e) => warn(&format!("pharos: video record not saved error={e}")),
    }
    result
}

/// Changes the record of `item`; false when it is gone (removed meanwhile).
fn update(user: &str, item: &str, f: impl FnOnce(&mut KeptVideo)) -> bool {
    with_list(
        |list| match list.iter_mut().find(|v| v.user == user && v.item == item) {
            Some(v) => {
                f(v);
                true
            }
            None => false,
        },
    )
}

fn origin() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}

fn caches() -> Result<web_sys::CacheStorage, String> {
    web_sys::window()
        .ok_or("no window")?
        .caches()
        .map_err(|e| format!("Cache Storage is unavailable: {}", describe(&e)))
}

async fn open_cache(user: &str) -> Result<web_sys::Cache, String> {
    let cache = JsFuture::from(caches()?.open(&cache_name(user)))
        .await
        .map_err(|e| describe(&e))?;
    cache.dyn_into().map_err(|e| describe(&e))
}

/// Tells the service worker the set of video caches changed.
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
    let _ = js_sys::Reflect::set(&message, &"kind".into(), &SW_KIND.into());
    let _ = js_sys::Reflect::set(&message, &"event".into(), &"videos-changed".into());
    if let Err(e) = worker.post_message(&message) {
        warn(&format!(
            "pharos: video worker note failed error={}",
            describe(&e)
        ));
    }
}

fn request_persist() {
    if PERSIST_ASKED.replace(true) {
        return;
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(promise) = window.navigator().storage().persist() else {
        info("pharos: video persist unavailable");
        return;
    };
    wasm_bindgen_futures::spawn_local(async move {
        match JsFuture::from(promise).await {
            Ok(v) => info(&format!(
                "pharos: video persist granted={}",
                v.as_bool().unwrap_or(false)
            )),
            Err(e) => warn(&format!(
                "pharos: video persist failed error={}",
                describe(&e)
            )),
        }
    });
}

/// Free space as the browser reports it (quota minus usage), refreshing the
/// value [`VideoStore::free_bytes`] returns.
async fn measure_free() -> Option<u64> {
    let promise = web_sys::window()?.navigator().storage().estimate().ok()?;
    let v = JsFuture::from(promise).await.ok()?;
    let num = |key: &str| js_sys::Reflect::get(&v, &key.into()).ok()?.as_f64();
    let free = (num("quota")? - num("usage")?).max(0.0) as u64;
    FREE.with(|c| c.set(free));
    Some(free)
}

/// Why a download stopped before it was ready.
enum Stop {
    /// Removed while running: nothing more to record.
    Gone,
    /// Over the cap or short of space; stored files are deleted.
    Refused(String),
    /// A file could not be fetched or stored; stored files stay for a retry.
    Failed(String),
}

enum FetchErr {
    Status(u16),
    Network(String),
}

async fn fetch_once(url: &str) -> Result<web_sys::Response, FetchErr> {
    let window = web_sys::window().ok_or_else(|| FetchErr::Network("no window".into()))?;
    let response: web_sys::Response = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| FetchErr::Network(describe(&e)))?
        .dyn_into()
        .map_err(|e| FetchErr::Network(describe(&e)))?;
    if response.ok() {
        Ok(response)
    } else {
        Err(FetchErr::Status(response.status()))
    }
}

fn wanted(user: &str, item: &str) -> bool {
    with_list(|l| {
        find(l, user, item).is_some_and(|v| matches!(v.state, KeptState::Downloading { .. }))
    })
}

/// Fetches `url`, retrying with backoff while the failure is one a retry
/// can mend, and stops once the download has been removed.
async fn fetch_retry(user: &str, item: &str, url: &str) -> Result<web_sys::Response, Stop> {
    let mut attempt = 0u32;
    loop {
        if !wanted(user, item) {
            return Err(Stop::Gone);
        }
        let err = match fetch_once(url).await {
            Ok(r) => return Ok(r),
            Err(e) => e,
        };
        attempt += 1;
        let (status, shown) = match &err {
            FetchErr::Status(s) => (Some(*s), s.to_string()),
            FetchErr::Network(m) => (None, format!("network ({m})")),
        };
        warn(&format!(
            "pharos: video keep file failed item={item} url_path={} status={shown} attempt={attempt}",
            url_path(url)
        ));
        match (is_retryable(status), retry_delay_secs(attempt)) {
            (true, Some(secs)) => platform::sleep_ms(secs as u32 * 1000).await,
            _ => return Err(Stop::Failed(format!("{} answered {shown}", url_path(url)))),
        }
    }
}

enum PutErr {
    /// The body transfer broke: fetching again can mend it.
    Transfer(String),
    /// The store refused it (quota and the like).
    Store(String),
}

async fn put(
    cache: &web_sys::Cache,
    url: &str,
    response: &web_sys::Response,
) -> Result<(), PutErr> {
    JsFuture::from(cache.put_with_str(url, response))
        .await
        .map(|_| ())
        .map_err(|e| {
            let name = js_sys::Reflect::get(&e, &"name".into())
                .ok()
                .and_then(|n| n.as_string())
                .unwrap_or_default();
            let reason = format!("{name}: {}", describe(&e));
            if put_is_retryable(&name) {
                PutErr::Transfer(reason)
            } else {
                PutErr::Store(reason)
            }
        })
}

/// Fetches `url` and stores it, fetching again (on the same backoff) when
/// the body breaks mid-transfer. With `read_back` the response comes back
/// with its body unread, for a playlist's text.
async fn fetch_put(
    cache: &web_sys::Cache,
    user: &str,
    item: &str,
    url: &str,
    read_back: bool,
) -> Result<(web_sys::Response, u64), Stop> {
    let mut attempt = 0u32;
    loop {
        let response = fetch_retry(user, item, url).await?;
        let len = length_of(&response);
        let (stored, back) = if read_back {
            let copy = response.clone().map_err(|e| Stop::Failed(describe(&e)))?;
            (copy, Some(response))
        } else {
            (response, None)
        };
        let err = match put(cache, url, &stored).await {
            Ok(()) => {
                let r = match back {
                    Some(r) => r,
                    None => stored,
                };
                return Ok((r, len));
            }
            Err(e) => e,
        };
        attempt += 1;
        match err {
            PutErr::Store(reason) => {
                let reason = format!("could not store {}: {reason}", url_path(url));
                warn(&format!(
                    "pharos: video keep store failed item={item} reason={reason}"
                ));
                return Err(Stop::Failed(reason));
            }
            PutErr::Transfer(reason) => {
                warn(&format!(
                    "pharos: video keep file failed item={item} url_path={} status=body ({reason}) attempt={attempt}",
                    url_path(url)
                ));
                match retry_delay_secs(attempt) {
                    Some(secs) => platform::sleep_ms(secs as u32 * 1000).await,
                    None => {
                        return Err(Stop::Failed(format!(
                            "{} body transfer failed: {reason}",
                            url_path(url)
                        )))
                    }
                }
            }
        }
    }
}

fn length_of(response: &web_sys::Response) -> u64 {
    response
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// A playlist's text, from the cache when stored already, else fetched and
/// stored.
async fn playlist(
    cache: &web_sys::Cache,
    user: &str,
    item: &str,
    url: &str,
) -> Result<String, Stop> {
    let cached = JsFuture::from(cache.match_with_str(url)).await.ok();
    let response = match cached.and_then(|v| v.dyn_into::<web_sys::Response>().ok()) {
        Some(r) => r,
        None => fetch_put(cache, user, item, url, true).await?.0,
    };
    let promise = response.text().map_err(|e| Stop::Failed(describe(&e)))?;
    JsFuture::from(promise)
        .await
        .map_err(|e| Stop::Failed(describe(&e)))?
        .as_string()
        .ok_or_else(|| Stop::Failed(format!("{} is not text", url_path(url))))
}

async fn cached_urls(cache: &web_sys::Cache) -> HashSet<String> {
    let Ok(keys) = JsFuture::from(cache.keys()).await else {
        return HashSet::new();
    };
    js_sys::Array::from(&keys)
        .iter()
        .filter_map(|k| js_sys::Reflect::get(&k, &"url".into()).ok()?.as_string())
        .collect()
}

async fn cached_length(cache: &web_sys::Cache, url: &str) -> u64 {
    match JsFuture::from(cache.match_with_str(url)).await {
        Ok(v) => v
            .dyn_into::<web_sys::Response>()
            .map_or(0, |r| length_of(&r)),
        Err(_) => 0,
    }
}

async fn delete_files(user: &str, files: &[String]) {
    let Ok(cache) = open_cache(user).await else {
        return;
    };
    for url in files {
        let _ = JsFuture::from(cache.delete_with_str(url)).await;
    }
    if cached_urls(&cache).await.is_empty() {
        if let Ok(storage) = caches() {
            let _ = JsFuture::from(storage.delete(&cache_name(user))).await;
        }
    }
}

/// The download, from planning to Ready. `resume` is a restart after the
/// page closed: the size check is not repeated against bytes already kept.
async fn download(user: &str, item: &str, resume: bool) -> Result<u64, Stop> {
    let record = with_list(|l| find(l, user, item).cloned()).ok_or(Stop::Gone)?;
    let cache = open_cache(user).await.map_err(Stop::Failed)?;
    notify_worker();
    let origin = origin();
    let master = absolute_url(&origin, &record.master_url);

    let text = playlist(&cache, user, item, &master).await?;
    let plan = plan_master(&master, &text)
        .map_err(|e| Stop::Failed(format!("master playlist not understood: {e}")))?;
    let mut stored = vec![master.clone()];
    let mut files: Vec<String> = Vec::new();
    let mut duration_ms = 0u64;
    let mut short_tails: HashSet<String> = HashSet::new();
    for url in &plan.media_playlists {
        let text = playlist(&cache, user, item, url).await?;
        stored.push(url.clone());
        let media = plan_media(url, &text)
            .map_err(|e| Stop::Failed(format!("{} not understood: {e}", url_path(url))))?;
        duration_ms = duration_ms.max(media.duration_ms);
        short_tails.extend(media.short_tail);
        files.extend(media.files);
    }
    let mut all = stored.clone();
    all.extend(files);
    all.extend(
        record
            .subtitles
            .iter()
            .map(|s| absolute_url(&origin, &s.url)),
    );
    let mut seen = HashSet::new();
    all.retain(|f| seen.insert(f.clone()));

    let estimate = estimate_bytes(plan.bandwidth_bps, duration_ms);
    info(&format!(
        "pharos: video keep start item={item} quality={} files={} estimate={estimate}",
        record.quality_bps,
        all.len()
    ));
    let cached = cached_urls(&cache).await;
    let mut stored_bytes = 0u64;
    for url in all.iter().filter(|f| cached.contains(*f)) {
        stored_bytes += cached_length(&cache, url).await;
    }
    if needs_admission(resume, record.estimated_bytes) {
        let free = measure_free().await.unwrap_or(u64::MAX);
        let used = with_list(|l| reserved_by(l, user, Some(item)));
        let needed = still_needed(estimate, stored_bytes);
        // What this download already holds counts against the cap; only
        // what is still to fetch needs free space.
        if let Err(refusal) = fits(
            needed,
            used.saturating_add(stored_bytes),
            DEFAULT_CAP_BYTES,
            free,
        ) {
            warn(&format!(
                "pharos: video keep refused item={item} reason={refusal} used={used} free={free} resume={resume}"
            ));
            delete_files(user, &stored).await;
            return Err(Stop::Refused(refusal.to_string()));
        }
    }
    let total = u32::try_from(all.len()).unwrap_or(u32::MAX);
    let planned = all.clone();
    let stale = stale_files(&record.files, &all);
    if !update(user, item, |v| {
        v.files = planned;
        v.estimated_bytes = estimate;
        v.state = KeptState::Downloading {
            done: 0,
            total,
            bytes: 0,
        };
    }) {
        return Err(Stop::Gone);
    }
    let _ = save();
    if !stale.is_empty() {
        info(&format!(
            "pharos: video keep dropped old files item={item} files={}",
            stale.len()
        ));
        delete_files(user, &stale).await;
    }

    let todo = missing_files(&all, &cached);
    let mut done = all.len() - todo.len();
    let mut bytes = stored_bytes;
    publish(user, item, done, total, bytes, true);

    let cache = &cache;
    let short_tails = &short_tails;
    let mut results = stream::iter(todo.iter().map(|url| async move {
        let len = match fetch_put(cache, user, item, url, false).await {
            Ok((_, len)) => len,
            // B1476: the sliver of a last segment can fail to encode; the
            // copy plays without it, ending a frame early.
            Err(Stop::Failed(reason)) if short_tails.contains(url) => {
                warn(&format!(
                    "pharos: video keep skipped short tail item={item} url_path={} reason={reason}",
                    url_path(url)
                ));
                return Ok(0);
            }
            Err(e) => return Err(e),
        };
        Ok::<u64, Stop>(len)
    }))
    .buffer_unordered(CONCURRENCY);
    while let Some(result) = results.next().await {
        bytes += result?;
        done += 1;
        publish(user, item, done, total, bytes, false);
    }
    drop(results);

    warm_player_script().await.map_err(|reason| {
        warn(&format!(
            "pharos: video keep player script failed item={item} reason={reason}"
        ));
        Stop::Failed(reason)
    })?;
    Ok(if bytes == 0 { estimate } else { bytes })
}

/// Records progress; saved at most once a second unless `force`.
fn publish(user: &str, item: &str, done: usize, total: u32, bytes: u64, force: bool) {
    let done = u32::try_from(done).unwrap_or(u32::MAX);
    let alive = update(user, item, |v| {
        v.state = KeptState::Downloading { done, total, bytes };
    });
    if alive && (force || progress_due(LAST_SAVE_MS.with(Cell::get), js_sys::Date::now())) {
        let _ = save();
    }
}

/// Fetches the player script so the service worker stores it, and checks it
/// did: an offline start cannot play without it. Errs naming the cause.
async fn warm_player_script() -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let response: web_sys::Response = JsFuture::from(window.fetch_with_str(platform::HLSJS_SRC))
        .await
        .map_err(|e| format!("player script fetch failed: {}", describe(&e)))?
        .dyn_into()
        .map_err(|e| describe(&e))?;
    if !response.ok() {
        return Err(format!(
            "player script {} answered {}",
            url_path(platform::HLSJS_SRC),
            response.status()
        ));
    }
    if let Ok(p) = response.blob() {
        let _ = JsFuture::from(p).await;
    }
    let key = absolute_url(&origin(), platform::HLSJS_SRC);
    let storage = caches()?;
    // The worker stores it after answering, so give it a moment.
    for attempt in 0..5 {
        let hit = JsFuture::from(storage.match_with_str(&key)).await;
        if hit.is_ok_and(|v| !v.is_undefined()) {
            info(&format!(
                "pharos: video keep player script cached attempt={attempt}"
            ));
            return Ok(());
        }
        platform::sleep_ms(300).await;
    }
    // No worker controls this page yet (a first visit): store it in the
    // shell's cache ourselves, where the worker looks for it offline.
    let keys: js_sys::Array = JsFuture::from(storage.keys())
        .await
        .map_err(|e| describe(&e))?
        .into();
    let shell = keys
        .iter()
        .filter_map(|k| k.as_string())
        .find(|k| k.starts_with("pharos-shell-"));
    let Some(shell) = shell else {
        return Err(format!(
            "player script {} not stored: no service worker cache",
            url_path(platform::HLSJS_SRC)
        ));
    };
    let cache: web_sys::Cache = JsFuture::from(storage.open(&shell))
        .await
        .map_err(|e| describe(&e))?
        .dyn_into()
        .map_err(|e| describe(&e))?;
    JsFuture::from(cache.add_with_str(&key))
        .await
        .map_err(|e| format!("player script not stored in {shell}: {}", describe(&e)))?;
    info(&format!(
        "pharos: video keep player script stored by page cache={shell}"
    ));
    Ok(())
}

async fn run(user: String, item: String, resume: bool) {
    match download(&user, &item, resume).await {
        Ok(bytes) => {
            if update(&user, &item, |v| {
                v.state = KeptState::Ready;
                v.estimated_bytes = bytes;
            }) {
                let _ = save();
                info(&format!(
                    "pharos: video keep ready item={item} bytes={bytes}"
                ));
            }
        }
        Err(Stop::Gone) => info(&format!(
            "pharos: video keep stopped item={item} reason=removed"
        )),
        Err(Stop::Refused(reason)) | Err(Stop::Failed(reason)) => {
            warn(&format!(
                "pharos: video keep failed item={item} reason={reason}"
            ));
            if update(&user, &item, |v| v.state = KeptState::Failed { reason }) {
                let _ = save();
            }
        }
    }
    notify_worker();
}

/// The browser's store of downloaded video.
pub struct WebVideoStore;

impl VideoStore for WebVideoStore {
    fn videos(&self, user: &str) -> Vec<KeptVideo> {
        with_list(|l| l.iter().filter(|v| v.user == user).cloned().collect())
    }

    fn start(&self, request: KeepVideoRequest) -> Result<(), KeepVideoRefusal> {
        if let Err(e) = caches() {
            warn(&format!(
                "pharos: video keep refused item={} reason={e}",
                request.item
            ));
            return Err(KeepVideoRefusal::Host(e));
        }
        let (user, item) = (request.user.clone(), request.item.clone());
        let mut previous = None;
        let started = with_list(|list| {
            check_not_kept(list, &user, &item)?;
            previous = find(list, &user, &item).cloned();
            let mut record = new_record(&request, js_sys::Date::new_0().to_iso_string().into());
            if let Some(old) = find(list, &user, &item) {
                record.files = old.files.clone();
            }
            insert_front(list, record);
            Ok(())
        });
        if let Err(refusal) = started {
            warn(&format!(
                "pharos: video keep refused item={item} reason={refusal}"
            ));
            return Err(refusal);
        }
        if let Err(e) = save() {
            // Undo: without a saved record a closed page would lose the
            // download and its files.
            with_list(|l| {
                take(l, &user, &item);
                if let Some(old) = previous {
                    l.insert(0, old);
                }
            });
            warn(&format!(
                "pharos: video keep refused item={item} reason={e}"
            ));
            return Err(KeepVideoRefusal::Host(format!(
                "the download record could not be saved: {e}"
            )));
        }
        request_persist();
        wasm_bindgen_futures::spawn_local(run(user, item, false));
        Ok(())
    }

    fn remove(&self, user: &str, item: &str) {
        let Some(record) = with_list(|l| take(l, user, item)) else {
            return;
        };
        let _ = save();
        info(&format!(
            "pharos: video keep remove item={item} files={}",
            record.files.len()
        ));
        let user = user.to_owned();
        wasm_bindgen_futures::spawn_local(async move {
            delete_files(&user, &record.files).await;
            notify_worker();
        });
    }

    fn save_position(&self, user: &str, item: &str, position_ms: u64) {
        if update(user, item, |v| v.position_ms = position_ms) {
            let _ = save();
        }
    }

    fn cap_bytes(&self) -> u64 {
        DEFAULT_CAP_BYTES
    }

    fn used_bytes(&self, user: &str) -> u64 {
        with_list(|l| used_by(l, user, None))
    }

    fn free_bytes(&self) -> u64 {
        FREE.with(Cell::get)
    }
}

/// Registers the store, restarts downloads the page left half done, and
/// refreshes the free-space figure. The browser build calls it once.
pub fn register() {
    if super::ports::register_video_store(Arc::new(WebVideoStore)).is_err() {
        warn("pharos: video store already registered");
        return;
    }
    let restart = with_list(|l| interrupted(l));
    wasm_bindgen_futures::spawn_local(async {
        let _ = measure_free().await;
    });
    if with_list(|l| l.iter().any(KeptVideo::ready)) {
        // A shell deploy drops the cached player script; fetch it again
        // while online so a ready video still plays offline.
        wasm_bindgen_futures::spawn_local(async {
            match warm_player_script().await {
                Ok(()) => info("pharos: video player script refreshed"),
                Err(e) => warn(&format!(
                    "pharos: video player script refresh failed reason={e}"
                )),
            }
        });
    }
    for v in restart {
        info(&format!(
            "pharos: video keep resume item={} files={}",
            v.item,
            v.files.len()
        ));
        wasm_bindgen_futures::spawn_local(run(v.user, v.item, true));
    }
}
