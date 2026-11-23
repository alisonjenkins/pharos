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
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures_util::stream::{self, StreamExt as _};
use pharos_hls::offline::{
    estimate_bytes, plan_master, plan_media, play_session, rebind, same_playlist,
};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::download_queue::{halt_for_status, Halt};
use super::kept_video::{
    absolute_url, apply_plan, cache_name, cap_from_stored, cap_selected, check_not_kept, find,
    fits, insert_front, interrupted, is_retryable, is_server_failure, missing_files,
    needs_admission, new_record, progress_due, put_is_retryable, renewal, renewed_record,
    reserved_by, retry_delay_secs, stale_files, still_needed, take, url_path, used_by,
    KeepVideoRefusal, KeepVideoRequest, KeptState, KeptVideo, RunIds, VideoStore, CAP_KEY,
    RECORDS_KEY,
};
use super::platform::{self, info, warn};

const SW_KIND: &str = "pharos-sw";
const CONCURRENCY: usize = 2;

thread_local! {
    static LIST: RefCell<Option<Vec<KeptVideo>>> = const { RefCell::new(None) };
    static FREE: Cell<u64> = const { Cell::new(0) };
    static PERSIST_ASKED: Cell<bool> = const { Cell::new(false) };
    static LAST_SAVE_MS: Cell<f64> = const { Cell::new(0.0) };
    /// The play session each renewed download fetches under, by user and
    /// item, when it is not the one its stored keys carry (ADR 0148).
    static LIVE: RefCell<HashMap<(String, String), String>> = RefCell::new(HashMap::new());
    /// Which run owns each download: a stale run touches nothing of its
    /// successor's.
    static RUNS: RefCell<RunIds> = RefCell::new(RunIds::default());
}

/// The URL a stored key is fetched from: the key itself, or the key under
/// the download's renewed play session.
fn fetch_url(user: &str, item: &str, key: &str) -> String {
    LIVE.with(
        |l| match l.borrow().get(&(user.to_owned(), item.to_owned())) {
            Some(live) => rebind(key, live),
            None => key.to_owned(),
        },
    )
}

fn live_session(user: &str, item: &str) -> Option<String> {
    LIVE.with(|l| l.borrow().get(&(user.to_owned(), item.to_owned())).cloned())
}

fn set_live(user: &str, item: &str, live: Option<String>) {
    LIVE.with(|l| {
        let mut l = l.borrow_mut();
        let key = (user.to_owned(), item.to_owned());
        match live {
            Some(s) => l.insert(key, s),
            None => l.remove(&key),
        };
    });
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

/// The cap in force: the viewer's stored choice, else the default. Read
/// afresh each time, so a cap another tab set applies here too.
fn current_cap() -> u64 {
    let stored = local_storage()
        .and_then(|s| s.get_item(CAP_KEY).map_err(|e| describe(&e)))
        .unwrap_or_else(|e| {
            warn(&format!("pharos: video cap unreadable reason={e}"));
            None
        });
    cap_from_stored(stored.as_deref())
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
    /// The server answered every attempt at a file with an error of its own
    /// (5xx): what a short tail it cannot encode gets (B1484). It halts the
    /// queue like any other error a later try can mend (ADR 0148), so the
    /// files stay and the queue tries again by itself.
    ServerFailed(Halt, String),
    /// Stopped by something a later try can mend (ADR 0148): the files stay
    /// and the queue tries again by itself.
    Halted(Halt, String),
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

/// Whether run `run` still owns the item.
fn owns(user: &str, item: &str, run: u64) -> bool {
    RUNS.with(|r| r.borrow().is_current(user, item, run))
}

fn wanted(user: &str, item: &str, run: u64) -> bool {
    owns(user, item, run)
        && with_list(|l| {
            find(l, user, item).is_some_and(|v| matches!(v.state, KeptState::Downloading { .. }))
        })
}

/// Fetches `url`, retrying with backoff while the failure is one a retry
/// can mend, and stops once the download has been removed.
async fn fetch_retry(
    user: &str,
    item: &str,
    run: u64,
    url: &str,
) -> Result<web_sys::Response, Stop> {
    let mut attempt = 0u32;
    loop {
        if !wanted(user, item, run) {
            return Err(Stop::Gone);
        }
        let err = match fetch_once(&fetch_url(user, item, url)).await {
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
            _ => {
                let reason = format!("{} answered {shown}", url_path(url));
                return Err(match halt_for_status(status) {
                    Some(halt) if is_server_failure(status) => Stop::ServerFailed(halt, reason),
                    Some(halt) => Stop::Halted(halt, reason),
                    None => Stop::Failed(reason),
                });
            }
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
    run: u64,
    url: &str,
    read_back: bool,
) -> Result<(web_sys::Response, u64), Stop> {
    let mut attempt = 0u32;
    loop {
        let response = fetch_retry(user, item, run, url).await?;
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
    run: u64,
    url: &str,
) -> Result<String, Stop> {
    let cached = JsFuture::from(cache.match_with_str(url)).await.ok();
    let response = match cached.and_then(|v| v.dyn_into::<web_sys::Response>().ok()) {
        Some(r) => r,
        None => fetch_put(cache, user, item, run, url, true).await?.0,
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
async fn download(user: &str, item: &str, run: u64, resume: bool) -> Result<u64, Stop> {
    let record = with_list(|l| find(l, user, item).cloned()).ok_or(Stop::Gone)?;
    let cache = open_cache(user).await.map_err(Stop::Failed)?;
    notify_worker();
    let origin = origin();
    let master = absolute_url(&origin, &record.master_url);
    // ADR 0148: fetching under a renewed play session. Every playlist must
    // be stored already, so the plan comes from the stored text and names
    // the stored keys.
    let renewing = live_session(user, item)
        .zip(play_session(&record.master_url).map(str::to_owned))
        .filter(|(live, kept)| live != kept);
    let mut texts: Vec<(String, String)> = Vec::new();
    let mut inits: Vec<String> = Vec::new();
    if renewing.is_some() && !is_cached(&cache, &master).await {
        return Err(Stop::Halted(
            Halt::StreamChanged,
            format!("{} was never stored", url_path(&master)),
        ));
    }

    let text = playlist(&cache, user, item, run, &master).await?;
    texts.push((master.clone(), text.clone()));
    let plan = plan_master(&master, &text)
        .map_err(|e| Stop::Failed(format!("master playlist not understood: {e}")))?;
    let mut stored = vec![master.clone()];
    let mut files: Vec<String> = Vec::new();
    let mut duration_ms = 0u64;
    let mut short_tails: HashSet<String> = HashSet::new();
    for url in &plan.media_playlists {
        if renewing.is_some() && !is_cached(&cache, url).await {
            return Err(Stop::Halted(
                Halt::StreamChanged,
                format!("{} was never stored", url_path(url)),
            ));
        }
        let text = playlist(&cache, user, item, run, url).await?;
        stored.push(url.clone());
        let media = plan_media(url, &text)
            .map_err(|e| Stop::Failed(format!("{} not understood: {e}", url_path(url))))?;
        if renewing.is_some() {
            orphan_check(&cache, item, &media).await?;
        }
        duration_ms = duration_ms.max(media.duration_ms);
        short_tails.extend(media.short_tail);
        files.extend(media.files);
        inits.extend(media.inits);
        texts.push((url.clone(), text));
    }
    if let Some((live, kept)) = &renewing {
        verify_renewal(&cache, user, item, run, &texts, &inits, kept, live).await?;
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
        "pharos: video keep start item={item} quality_bps={} files={} estimate={estimate}",
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
            current_cap(),
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
    // The run that planned this may have been replaced while the playlists
    // were read (a pause, then a start): its plan does not touch the
    // record its successor owns.
    if !owns(user, item, run) {
        info(&format!(
            "pharos: video keep plan item={item} run={run} verdict=dropped reason=superseded_by_newer_run"
        ));
        return Err(Stop::Gone);
    }
    let mut running = false;
    if !update(user, item, |v| {
        running = apply_plan(v, planned, estimate, total);
    }) {
        return Err(Stop::Gone);
    }
    if !running {
        // A pause landed while the plan was made: it stays paused, with the
        // plan's files recorded for the resume.
        let _ = save();
        info(&format!(
            "pharos: video keep plan item={item} verdict=kept_paused files={}",
            all.len()
        ));
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
    publish(user, item, run, done, total, bytes, true);

    let cache = &cache;
    let short_tails = &short_tails;
    let mut results = stream::iter(todo.iter().map(|url| async move {
        let len = match fetch_put(cache, user, item, run, url, false).await {
            Ok((_, len)) => len,
            // B1476: the sliver of a last segment can fail to encode; the
            // copy plays without it, ending a frame early. Only the server's
            // own refusal says so: a 401 or a lost network is not (B1484).
            Err(Stop::ServerFailed(_, reason)) if short_tails.contains(url) => {
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
        publish(user, item, run, done, total, bytes, false);
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

async fn is_cached(cache: &web_sys::Cache, url: &str) -> bool {
    JsFuture::from(cache.match_with_str(url))
        .await
        .is_ok_and(|v| !v.is_undefined())
}

async fn body_bytes(response: &web_sys::Response) -> Result<Vec<u8>, String> {
    let promise = response.array_buffer().map_err(|e| describe(&e))?;
    let buffer = JsFuture::from(promise).await.map_err(|e| describe(&e))?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// A segment stored without its init came from the ended session's encoder,
/// and nothing stored is left to check the renewed init against: such a
/// download is [`Halt::StreamChanged`], not spliced (ADR 0148).
async fn orphan_check(
    cache: &web_sys::Cache,
    item: &str,
    media: &pharos_hls::offline::MediaPlan,
) -> Result<(), Stop> {
    for init in &media.inits {
        if is_cached(cache, init).await {
            continue;
        }
        let mut segments = 0usize;
        for file in media.files.iter().filter(|f| !media.inits.contains(f)) {
            if is_cached(cache, file).await {
                segments += 1;
            }
        }
        if segments > 0 {
            let what = format!(
                "url_path={} init_never_stored segments_stored={segments}",
                url_path(init)
            );
            warn(&format!(
                "pharos: video keep renewed item={item} verdict=stream_changed {what}"
            ));
            return Err(Stop::Halted(
                Halt::StreamChanged,
                format!("the stream changed: {what}"),
            ));
        }
    }
    Ok(())
}

/// Checks the renewed session hands out the stored stream (ADR 0148): each
/// playlist says the same once its session ids are put back, and each
/// stored init segment is the same bytes. Anything else is
/// [`Halt::StreamChanged`], naming the first difference, and the queue
/// starts the download afresh.
#[allow(clippy::too_many_arguments)]
async fn verify_renewal(
    cache: &web_sys::Cache,
    user: &str,
    item: &str,
    run: u64,
    texts: &[(String, String)],
    inits: &[String],
    kept: &str,
    live: &str,
) -> Result<(), Stop> {
    let changed = |what: String| {
        warn(&format!(
            "pharos: video keep renewed item={item} verdict=stream_changed {what}"
        ));
        Stop::Halted(Halt::StreamChanged, format!("the stream changed: {what}"))
    };
    for (key, stored) in texts {
        let response = fetch_retry(user, item, run, key).await?;
        let renewed = JsFuture::from(response.text().map_err(|e| Stop::Failed(describe(&e)))?)
            .await
            .map_err(|e| Stop::Failed(describe(&e)))?
            .as_string()
            .unwrap_or_default();
        if let Err(m) = same_playlist(stored, &renewed, kept, live) {
            return Err(changed(format!("url_path={} {m}", url_path(key))));
        }
    }
    let mut compared = 0usize;
    for key in inits {
        let Ok(hit) = JsFuture::from(cache.match_with_str(key)).await else {
            continue;
        };
        let Ok(stored) = hit.dyn_into::<web_sys::Response>() else {
            continue;
        };
        let stored = body_bytes(&stored).await.map_err(Stop::Failed)?;
        let renewed = fetch_retry(user, item, run, key).await?;
        let renewed = body_bytes(&renewed).await.map_err(Stop::Failed)?;
        if stored != renewed {
            return Err(changed(format!(
                "url_path={} init_bytes kept={} renewed={}",
                url_path(key),
                stored.len(),
                renewed.len()
            )));
        }
        compared += 1;
    }
    info(&format!(
        "pharos: video keep renewed item={item} verdict=same_stream playlists={} inits={compared}",
        texts.len()
    ));
    Ok(())
}

/// Records progress; saved at most once a second unless `force`.
fn publish(user: &str, item: &str, run: u64, done: usize, total: u32, bytes: u64, force: bool) {
    if !owns(user, item, run) {
        return;
    }
    let done = u32::try_from(done).unwrap_or(u32::MAX);
    let alive = update(user, item, |v| {
        // A pause landed meanwhile: it stays paused.
        if matches!(v.state, KeptState::Downloading { .. }) {
            v.state = KeptState::Downloading { done, total, bytes };
        }
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

async fn run(user: String, item: String, id: u64, resume: bool) {
    let outcome = download(&user, &item, id, resume).await;
    // An older run (the download was paused and started again) ends without
    // touching its successor's session, progress or record.
    if !RUNS.with(|r| r.borrow_mut().release(&user, &item, id)) {
        info(&format!(
            "pharos: video keep stopped item={item} run={id} reason=superseded_by_newer_run"
        ));
        notify_worker();
        return;
    }
    set_live(&user, &item, None);
    // A pause or a removal that landed while the run ended leaves its
    // record as the viewer set it.
    let running = |v: &KeptVideo| matches!(v.state, KeptState::Downloading { .. });
    match outcome {
        Ok(bytes) => {
            let mut applied = false;
            update(&user, &item, |v| {
                if running(v) {
                    v.state = KeptState::Ready;
                    v.estimated_bytes = bytes;
                    v.halt = None;
                    applied = true;
                }
            });
            if applied {
                let _ = save();
                info(&format!(
                    "pharos: video keep ready item={item} bytes={bytes}"
                ));
            }
        }
        Err(Stop::Gone) => info(&format!(
            "pharos: video keep stopped item={item} reason=not_downloading"
        )),
        Err(Stop::Refused(reason) | Stop::Failed(reason)) => {
            warn(&format!(
                "pharos: video keep failed item={item} reason={reason} halt=none"
            ));
            let mut applied = false;
            update(&user, &item, |v| {
                if running(v) {
                    v.transferred = v.bytes();
                    v.state = KeptState::Failed { reason };
                    v.halt = None;
                    applied = true;
                }
            });
            if applied {
                let _ = save();
            }
        }
        Err(Stop::Halted(halt, reason) | Stop::ServerFailed(halt, reason)) => {
            warn(&format!(
                "pharos: video keep failed item={item} reason={reason} halt={}",
                halt.label()
            ));
            let mut applied = false;
            update(&user, &item, |v| {
                if running(v) {
                    v.transferred = v.bytes();
                    v.state = KeptState::Failed { reason };
                    v.halt = Some(halt);
                    applied = true;
                }
            });
            if applied {
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
        let mut live = None;
        let started = with_list(|list| {
            check_not_kept(list, &user, &item)?;
            previous = find(list, &user, &item).cloned();
            let now: String = js_sys::Date::new_0().to_iso_string().into();
            let record = match find(list, &user, &item) {
                // ADR 0148: the same stream under a new play session keeps
                // its stored keys and fetches them under the new session.
                Some(old) => match renewal(old, &request) {
                    Some(session) => {
                        live = Some(session);
                        renewed_record(old, &request, now)
                    }
                    None => {
                        let mut record = new_record(&request, now);
                        record.files = old.files.clone();
                        record
                    }
                },
                None => new_record(&request, now),
            };
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
        let renewing = live.is_some();
        info(&format!(
            "pharos: video keep renewal item={item} verdict={} files_kept={}",
            if renewing { "renewing" } else { "fresh" },
            previous
                .as_ref()
                .map_or(0, |p| if renewing { p.files.len() } else { 0 })
        ));
        set_live(&user, &item, live);
        let id = RUNS.with(|r| r.borrow_mut().claim(&user, &item));
        wasm_bindgen_futures::spawn_local(run(user, item, id, renewing));
        Ok(())
    }

    fn pause(&self, user: &str, item: &str) -> Result<(), String> {
        let mut paused = false;
        update(user, item, |v| {
            if matches!(v.state, KeptState::Downloading { .. }) {
                v.transferred = v.bytes();
                v.state = KeptState::Failed {
                    reason: "paused".to_owned(),
                };
                v.halt = Some(Halt::Paused);
                paused = true;
            }
        });
        if !paused {
            return Err(format!("{item} is not downloading"));
        }
        save()?;
        info(&format!("pharos: video keep paused item={item}"));
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
        current_cap()
    }

    fn set_cap_bytes(&self, bytes: u64) -> Result<(), String> {
        let option = cap_selected(bytes);
        if option.is_empty() {
            return Err(format!("{bytes} bytes is not a cap this page offers"));
        }
        local_storage().and_then(|s| s.set_item(CAP_KEY, &option).map_err(|e| describe(&e)))
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
        let id = RUNS.with(|r| r.borrow_mut().claim(&v.user, &v.item));
        wasm_bindgen_futures::spawn_local(run(v.user, v.item, id, true));
    }
}
