//! Books kept on this device, in the browser (spec 036 FR-004 to FR-006,
//! ADR-0110): Cache Storage holds the bytes under `pharos-books-<user>`,
//! one small `localStorage` value records what is kept (`kept.rs`).
//!
//! The page talks to Cache Storage itself; the service worker never serves
//! a book (V1133), so a book is only ever read by the signed-in page.
//!
//! Cost model, for TVs and headsets: nothing runs at start-up but the one
//! `localStorage` read the offline start and the title page make, and a TV
//! with no kept book finds no value. The bytes are streamed into the cache
//! by the browser, never copied through wasm memory.

use std::cell::Cell;

use dioxus::prelude::*;
use pharos_client::api::{BookFormat, ItemRef};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::app::{call, this_device, Ctx};
use super::kept::{
    auto_keep_skip, book_key, cache_name, is_books_cache, keep_refusal, plan_eviction, AutoKeep,
    KeptBook, KeptFormat, KeptStore, Reason, Why, SETTINGS_KEY, STORE_KEY,
};
use super::kept_view::{AutoKeepSettings, KeepControl, KeptList, OfflineView};
use super::platform;
use super::reader::size_cap;

/// The PDF module's files beside the loader (`flake.nix` installs them under
/// `assets/pdf`). The service worker stores a GET of any of them (ADR-0110).
const PDF_MODULE: [&str; 3] = [
    "/ui/assets/pdf/loader.js",
    "/ui/assets/pdf/quire_pdf.js",
    "/ui/assets/pdf/quire_pdf_bg.wasm",
];

thread_local! {
    static PERSIST_ASKED: Cell<bool> = const { Cell::new(false) };
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

/// The record of kept books, or `None` when nothing is kept (or the record
/// cannot be read, which is logged).
pub fn read() -> Option<KeptStore> {
    read_checked().ok().flatten()
}

/// How many books are kept, or `None` when the record cannot be read: the
/// caches may still hold books, so a caller must not take that as none.
pub fn count() -> Option<usize> {
    read_checked()
        .ok()
        .map(|store| store.map_or(0, |s| s.books.len()))
}

/// [`read`], telling an unreadable or damaged record (`Err`, logged) from
/// none at all.
fn read_checked() -> Result<Option<KeptStore>, ()> {
    let text = match local_storage().and_then(|s| s.get_item(STORE_KEY).map_err(|e| describe(&e))) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(None),
        Err(e) => {
            platform::warn(&format!("pharos: kept record unreadable error={e}"));
            return Err(());
        }
    };
    match KeptStore::parse(&text) {
        Ok(store) => Ok(store),
        Err(e) => {
            platform::warn(&format!("pharos: kept record damaged error={e}"));
            Err(())
        }
    }
}

/// What keep and remove say when the record of kept books cannot be read.
const UNREADABLE: &str = "The record of books kept on this device could not be read.";

fn write(store: &KeptStore) {
    let result = local_storage().and_then(|s| {
        match store.to_text() {
            Some(text) => s.set_item(STORE_KEY, &text),
            None => s.remove_item(STORE_KEY),
        }
        .map_err(|e| describe(&e))
    });
    if let Err(e) = result {
        platform::warn(&format!("pharos: kept record not saved error={e}"));
    }
}

/// Unix milliseconds on this device's clock.
fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// The automatic-keeping settings of this device: read on every book open
/// and every opening of the list, so a change applies without a reload.
pub fn settings() -> AutoKeep {
    let text = local_storage().and_then(|s| s.get_item(SETTINGS_KEY).map_err(|e| describe(&e)));
    match text {
        Ok(text) => AutoKeep::parse(text.as_deref()),
        Err(e) => {
            platform::warn(&format!("pharos: kept settings unreadable error={e}"));
            AutoKeep::default()
        }
    }
}

fn save_settings(settings: &AutoKeep) {
    let result = local_storage().and_then(|s| match settings.to_text() {
        Some(text) => s.set_item(SETTINGS_KEY, &text).map_err(|e| describe(&e)),
        None => Err("settings could not be written".to_owned()),
    });
    match result {
        Ok(()) => platform::info(&format!(
            "pharos: kept settings reading={} recent={} last_n={} budget_mib={}",
            settings.reading, settings.recent, settings.last_n, settings.budget_mib
        )),
        Err(e) => platform::warn(&format!("pharos: kept settings not saved error={e}")),
    }
}

/// Removes the automatic books the settings no longer keep: policy turned
/// off, a week unopened, beyond the last N, over the space. Nothing is read
/// but the record, and nothing is done when nothing is kept.
pub async fn enforce(trigger: &str) {
    let Some(store) = read() else {
        return;
    };
    let settings = settings();
    let plan = plan_eviction(&store.books, &settings, now_ms());
    if !plan.relabel.is_empty() {
        // Read again: the record is the page's to write, with no wait between.
        let mut store = read().unwrap_or(store);
        for (id, reason) in &plan.relabel {
            if let Some(book) = store.books.iter_mut().find(|b| b.id == *id) {
                book.reason = *reason;
                platform::info(&format!(
                    "pharos: kept relabel item={id} reason={} trigger={trigger}",
                    reason.word()
                ));
            }
        }
        write(&store);
    }
    for gone in plan.evict {
        match remove(&gone.id).await {
            Ok(()) if gone.why == Why::Expired => platform::info(&format!(
                "pharos: kept expire item={} bytes={} trigger={trigger}",
                gone.id, gone.bytes
            )),
            Ok(()) => platform::info(&format!(
                "pharos: kept evict item={} why={} bytes={} trigger={trigger}",
                gone.id,
                gone.why.label(),
                gone.bytes
            )),
            Err(e) => platform::warn(&format!(
                "pharos: kept evict failed item={} why={} error={e}",
                gone.id,
                gone.why.label()
            )),
        }
    }
}

/// Whether the browser says it has no connection. A phone in a tunnel often
/// says it has one, so the callers that wait on the server also bound the wait
/// ([`within`]).
pub fn is_offline() -> bool {
    web_sys::window().is_some_and(|w| !w.navigator().on_line())
}

/// How long the reader waits for the server's word on a book it already
/// opened from the device (spec 036 FR-006).
pub const SERVER_CHECK_MS: u32 = 3_000;

/// `fut`'s output, or `None` once `ms` have passed.
pub async fn within<T>(ms: u32, fut: impl std::future::Future<Output = T>) -> Option<T> {
    use futures_util::future::{select, Either};
    let wait = std::pin::pin!(platform::sleep_ms(ms));
    match select(std::pin::pin!(fut), wait).await {
        Either::Left((out, _)) => Some(out),
        Either::Right(_) => None,
    }
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

/// Asks the browser not to evict what is kept, once per page, and logs the
/// answer.
fn request_persist() {
    if PERSIST_ASKED.replace(true) {
        return;
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(promise) = window.navigator().storage().persist() else {
        platform::info("pharos: kept persist unavailable");
        return;
    };
    platform::spawn_detached(async move {
        match JsFuture::from(promise).await {
            Ok(v) => platform::info(&format!(
                "pharos: kept persist granted={}",
                v.as_bool().unwrap_or(false)
            )),
            Err(e) => platform::warn(&format!(
                "pharos: kept persist failed error={}",
                describe(&e)
            )),
        }
    });
}

async fn blob_size(response: &web_sys::Response) -> Result<u64, String> {
    let promise = response.blob().map_err(|e| describe(&e))?;
    let blob: web_sys::Blob = JsFuture::from(promise)
        .await
        .map_err(|e| describe(&e))?
        .dyn_into()
        .map_err(|e| describe(&e))?;
    Ok(blob.size() as u64)
}

async fn fetch(url: &str) -> Result<web_sys::Response, String> {
    let window = web_sys::window().ok_or("no window")?;
    let response: web_sys::Response = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| format!("The download failed: {}", describe(&e)))?
        .dyn_into()
        .map_err(|e| describe(&e))?;
    if !response.ok() {
        return Err(format!(
            "The server answered {} {}.",
            response.status(),
            response.status_text()
        ));
    }
    Ok(response)
}

/// Fetches the PDF module's files so the service worker stores them. Only a
/// page the worker controls can have them stored; a failure is logged and
/// the book is still kept (it opens once the module has been fetched online).
async fn ensure_pdf_module() {
    let controlled = web_sys::window()
        .map(|w| w.navigator().service_worker())
        .is_some_and(|c| c.controller().is_some());
    if !controlled {
        platform::warn("pharos: kept pdf module not stored: no service worker controls this page");
        return;
    }
    for url in PDF_MODULE {
        if let Err(e) = fetch(url).await {
            platform::warn(&format!(
                "pharos: kept pdf module fetch failed url={url} error={e}"
            ));
            return;
        }
    }
    platform::info("pharos: kept pdf module fetched");
}

/// Whether every file of the PDF module is in a cache, whichever: a page the
/// service worker does not control may have loaded the module without its
/// being stored.
async fn pdf_module_cached() -> bool {
    let Ok(storage) = caches() else {
        return false;
    };
    for url in PDF_MODULE {
        match JsFuture::from(storage.match_with_str(url)).await {
            Ok(found) if !found.is_undefined() => {}
            _ => return false,
        }
    }
    true
}

/// Stores a book on this device. The download goes straight into the cache;
/// a book over the weak-device cap is refused, before the body when the
/// server states its size and after it otherwise.
pub async fn keep(
    id: ItemRef,
    title: &str,
    format: KeptFormat,
    user: &str,
    place: Option<(String, f32)>,
) -> Result<KeptBook, String> {
    // An unreadable record is refused, not replaced: a fresh store would
    // drop every book the record still lists.
    let mut store = match read_checked().map_err(|()| UNREADABLE.to_owned())? {
        Some(store) if store.is_for(user) => store,
        Some(_) => {
            clear_all("another user").await;
            KeptStore::new(user)
        }
        None => KeptStore::new(user),
    };
    let cap = size_cap(this_device(), &platform::user_agent());
    let key = book_key(id);
    let response = fetch(&key).await?;
    let stated = response
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok());
    if let Some(line) = stated.and_then(|bytes| keep_refusal(bytes, cap)) {
        platform::warn(&format!(
            "pharos: kept refused item={id} bytes={} cap={cap:?}",
            stated.unwrap_or(0)
        ));
        return Err(line);
    }
    let cache = open_cache(user).await?;
    JsFuture::from(cache.put_with_str(&key, &response))
        .await
        .map_err(|e| format!("The book could not be stored: {}", describe(&e)))?;
    let stored = JsFuture::from(cache.match_with_str(&key))
        .await
        .map_err(|e| describe(&e))?;
    let stored: web_sys::Response = stored
        .dyn_into()
        .map_err(|_| "The book was not stored.".to_owned())?;
    let bytes = blob_size(&stored).await?;
    if let Some(line) = keep_refusal(bytes, cap) {
        // The server did not state a size: the stored copy is what shows it.
        let _ = JsFuture::from(cache.delete_with_str(&key)).await;
        platform::warn(&format!(
            "pharos: kept refused item={id} bytes={bytes} cap={cap:?}"
        ));
        return Err(line);
    }
    if format == KeptFormat::Pdf {
        ensure_pdf_module().await;
    }
    let book = KeptBook {
        id: id.to_string(),
        title: title.to_owned(),
        format,
        bytes,
        kept_at: String::from(js_sys::Date::new_0().to_iso_string()),
        reason: Reason::Hand,
        // Where the server has the reader, so a resume with no server starts
        // there; its time is unknown, so any place read here is later.
        fraction: place.as_ref().map_or(0.0, |p| p.1),
        locator: place.map(|p| p.0),
        read_at_ms: 0,
        opened_ms: 0,
    };
    store.keep(book.clone());
    write(&store);
    request_persist();
    platform::info(&format!(
        "pharos: kept item={id} bytes={bytes} format={} reason=hand total_bytes={}",
        format.label(),
        store.total_bytes()
    ));
    Ok(book)
}

/// Whether a book fetched for reading is worth keeping by a policy, so the
/// reader holds its bytes past opening only then.
pub fn wants_auto() -> bool {
    settings().reason_on_open().is_some()
}

/// Notes that the kept book `id` was opened now, and why it is kept now when
/// a policy applies (a book kept by hand stays so). A no-op for a book that
/// is not kept.
pub fn note_open(id: ItemRef) {
    let Some(mut store) = read() else {
        return;
    };
    if store.note_open(&id.to_string(), now_ms(), settings().reason_on_open()) {
        write(&store);
    }
}

/// Keeps a book the reader has just opened, from the bytes it already
/// fetched: they are written to the cache as they are, never fetched again
/// (FR-008). A failure, a full quota included, is logged and reading goes on.
/// `place` is where the reader is, so a resume with no server starts there.
/// Answers whether the book is kept now.
pub async fn auto_keep(
    id: ItemRef,
    title: &str,
    format: KeptFormat,
    user: &str,
    bytes: &js_sys::Uint8Array,
    place: (String, f32),
) -> bool {
    let settings = settings();
    let Some(reason) = settings.reason_on_open() else {
        return false;
    };
    let size = u64::from(bytes.length());
    if size == 0 {
        platform::warn(&format!(
            "pharos: kept auto skipped item={id} bytes=0 why=the buffer is empty"
        ));
        return false;
    }
    let cap = size_cap(this_device(), &platform::user_agent());
    if let Some(why) = auto_keep_skip(size, cap, &settings) {
        platform::info(&format!(
            "pharos: kept auto skipped item={id} bytes={size} why={why}"
        ));
        return false;
    }
    // The record is checked before any bytes go in: bytes it cannot list
    // would sit outside eviction for good.
    let before = match read_checked() {
        Ok(store) => store,
        Err(()) => {
            platform::warn(&format!(
                "pharos: kept auto skipped item={id} bytes={size} why=the kept record is unreadable"
            ));
            return false;
        }
    };
    if before.as_ref().is_some_and(|s| !s.is_for(user)) {
        clear_all("another user").await;
    }
    let was_kept = before.is_some_and(|s| s.is_for(user) && s.find(&id.to_string()).is_some());
    let key = book_key(id);
    let stored = async {
        let response =
            web_sys::Response::new_with_opt_buffer_source(Some(bytes)).map_err(|e| describe(&e))?;
        let cache = open_cache(user).await?;
        JsFuture::from(cache.put_with_str(&key, &response))
            .await
            .map_err(|e| describe(&e))
    };
    if let Err(e) = stored.await {
        platform::warn(&format!(
            "pharos: kept auto failed item={id} bytes={size} error={e}"
        ));
        return false;
    }
    // The record is read and written with no wait between, so a place noted
    // meanwhile is not lost.
    // An unreadable record is left alone: a fresh store would drop every
    // book it still lists.
    let mut store = match read_checked() {
        Ok(store) => store.unwrap_or_else(|| KeptStore::new(user)),
        Err(()) => {
            platform::warn(&format!(
                "pharos: kept auto failed item={id} bytes={size} error=record unreadable"
            ));
            // The bytes just cached are listed nowhere; a copy kept before
            // stays, as the record will still list it once it reads again.
            if !was_kept {
                forget_cached(user, &key).await;
            }
            return false;
        }
    };
    if store.find(&id.to_string()).is_some() {
        // Kept by hand meanwhile (another tab): that copy stands.
        return true;
    }
    let now = now_ms();
    store.keep(KeptBook {
        id: id.to_string(),
        title: title.to_owned(),
        format,
        bytes: size,
        kept_at: String::from(js_sys::Date::new_0().to_iso_string()),
        reason,
        locator: Some(place.0),
        fraction: place.1,
        read_at_ms: 0,
        opened_ms: now,
    });
    write(&store);
    request_persist();
    if format == KeptFormat::Pdf && !pdf_module_cached().await {
        ensure_pdf_module().await;
    }
    platform::info(&format!(
        "pharos: kept auto item={id} bytes={size} format={} reason={} total_bytes={}",
        format.label(),
        reason.word(),
        store.total_bytes()
    ));
    true
}

/// Deletes one cached book's bytes, logging a failure.
async fn forget_cached(user: &str, key: &str) {
    let deleted = async {
        let cache = open_cache(user).await?;
        JsFuture::from(cache.delete_with_str(key))
            .await
            .map_err(|e| describe(&e))
    };
    if let Err(e) = deleted.await {
        platform::warn(&format!(
            "pharos: kept auto cleanup failed key={key} error={e}"
        ));
    }
}

/// Removes a kept book from the device: its bytes and its record.
pub async fn remove(id: &str) -> Result<(), String> {
    // An unreadable record must not report the book gone while it stays.
    let Some(mut store) = read_checked().map_err(|()| UNREADABLE.to_owned())? else {
        return Ok(());
    };
    let Some(book) = store.find(id).cloned() else {
        return Ok(());
    };
    if let Some(item) = book.item() {
        let cache = open_cache(&store.user).await?;
        JsFuture::from(cache.delete_with_str(&book_key(item)))
            .await
            .map_err(|e| format!("The book could not be removed: {}", describe(&e)))?;
    }
    store.remove(id);
    let user = store.user.clone();
    write(&store);
    if store.books.is_empty() {
        // Nothing left: the cache goes too, so an empty one is not kept.
        let _ = JsFuture::from(caches()?.delete(&cache_name(&user))).await;
    }
    platform::info(&format!(
        "pharos: kept removed item={id} bytes={} total_bytes={}",
        book.bytes,
        store.total_bytes()
    ));
    Ok(())
}

/// Deletes every cache of kept books, whoever's, and the record: sign-out
/// (FR-005), or books found to be another user's.
pub async fn clear_all(why: &str) {
    let (books, bytes) = read().map_or((0, 0), |s| (s.books.len(), s.total_bytes()));
    let mut caches_deleted = 0_u32;
    match caches() {
        Ok(storage) => match JsFuture::from(storage.keys()).await {
            Ok(names) => {
                for name in js_sys::Array::from(&names)
                    .iter()
                    .filter_map(|n| n.as_string())
                    .filter(|n| is_books_cache(n))
                {
                    match JsFuture::from(storage.delete(&name)).await {
                        Ok(_) => caches_deleted = caches_deleted.saturating_add(1),
                        Err(e) => platform::warn(&format!(
                            "pharos: kept cache not deleted name={name} error={}",
                            describe(&e)
                        )),
                    }
                }
            }
            Err(e) => platform::warn(&format!(
                "pharos: kept caches unlisted error={}",
                describe(&e)
            )),
        },
        Err(e) => platform::warn(&format!("pharos: kept clear error={e}")),
    }
    if let Err(e) = local_storage().and_then(|s| s.remove_item(STORE_KEY).map_err(|e| describe(&e)))
    {
        platform::warn(&format!("pharos: kept record not removed error={e}"));
    }
    platform::info(&format!(
        "pharos: kept cleared why={why} books={books} bytes={bytes} caches={caches_deleted}"
    ));
}

/// A kept book's bytes, read from this user's cache. `None` when the book
/// is not there (evicted, or never kept), so the reader asks the network.
pub async fn cached_book(user: &str, id: ItemRef) -> Option<js_sys::Uint8Array> {
    let cache = open_cache(user).await.ok()?;
    let found = JsFuture::from(cache.match_with_str(&book_key(id)))
        .await
        .ok()?;
    let response: web_sys::Response = found.dyn_into().ok()?;
    let buffer = JsFuture::from(response.array_buffer().ok()?).await.ok()?;
    Some(js_sys::Uint8Array::new(&buffer))
}

/// Notes where the reader is in a kept book, for a resume with no server.
/// The save queue holds what still has to be sent; this is only the place.
/// A no-op for a book that is not kept.
pub fn note_position(id: ItemRef, locator: &str, fraction: f32, read_at_ms: i64) {
    let Some(mut store) = read() else {
        return;
    };
    if store.note_position(&id.to_string(), locator, fraction, read_at_ms) {
        write(&store);
    }
}

/// Called with the signed-in user once known: a record kept by someone else
/// is deleted, so a user never reads another's books (FR-005). Answers
/// whether it was.
pub fn reconcile(user: &str) -> bool {
    let Some(store) = read() else {
        return false;
    };
    if store.is_for(user) {
        return false;
    }
    platform::warn(&format!(
        "pharos: kept books belong to another user, clearing books={}",
        store.books.len()
    ));
    platform::spawn_detached(async {
        clear_all("another user").await;
    });
    true
}

/// Keep offline / Remove from device on a book's title page.
#[component]
pub fn KeepButton(
    id: ItemRef,
    title: String,
    format: BookFormat,
    place: Option<(String, f32)>,
) -> Element {
    let ctx = use_context::<Ctx>();
    let Some(kept_format) = KeptFormat::of(format) else {
        return rsx! {};
    };
    let mut kept = use_signal(move || read().is_some_and(|s| s.find(&id.to_string()).is_some()));
    let mut busy = use_signal(|| false);
    let mut message = use_signal(|| None::<String>);
    rsx! {
        KeepControl {
            kept: kept(),
            busy: busy(),
            message: message(),
            on_keep: move |()| {
                let Some(api) = ctx.api.peek().clone() else {
                    message.set(Some("Connect to the server to keep this book.".into()));
                    return;
                };
                let (title, place) = (title.clone(), place.clone());
                busy.set(true);
                message.set(None);
                spawn(async move {
                    let outcome = match api.me(&call("title", "keep")).await {
                        Ok(me) => keep(id, &title, kept_format, &me.id, place).await,
                        Err(e) => Err(format!("Could not tell who is signed in: {e}")),
                    };
                    match outcome {
                        Ok(_) => kept.set(true),
                        Err(line) => {
                            platform::warn(&format!("pharos: keep failed item={id} error={line}"));
                            message.set(Some(line));
                        }
                    }
                    busy.set(false);
                });
            },
            on_remove: move |()| {
                busy.set(true);
                message.set(None);
                spawn(async move {
                    match remove(&id.to_string()).await {
                        Ok(()) => kept.set(false),
                        Err(line) => message.set(Some(line)),
                    }
                    busy.set(false);
                });
            },
        }
    }
}

/// The last removal that failed: the book's id and why.
type RemoveFailure = Signal<Option<(String, String)>>;

/// The list as a signal the Remove buttons update.
fn use_store() -> (Signal<KeptStore>, EventHandler<String>, RemoveFailure) {
    let mut failed = use_signal(|| None::<(String, String)>);
    let store = use_signal(|| read().unwrap_or_else(|| KeptStore::new("")));
    // Opening the list is when books a policy no longer keeps go.
    use_hook(|| {
        spawn(async move {
            enforce("list").await;
            refresh(store);
        })
    });
    let on_remove = EventHandler::new(move |id: String| {
        spawn(async move {
            failed.set(None);
            match remove(&id).await {
                Ok(()) => {}
                Err(e) => {
                    platform::warn(&format!("pharos: kept remove failed item={id} error={e}"));
                    failed.set(Some((id, e)));
                }
            }
            refresh(store);
        });
    });
    (store, on_remove, failed)
}

/// Re-reads the list after a change. An unreadable record keeps the books
/// shown, so a failed removal still has its row to show the failure on.
fn refresh(mut store: Signal<KeptStore>) {
    if let Ok(read) = read_checked() {
        store.set(read.unwrap_or_else(|| KeptStore::new("")));
    }
}

/// Settings, On this device: the kept books, why, their size and a Remove
/// for each.
#[component]
pub fn OnThisDevice() -> Element {
    let (store, on_remove, failed) = use_store();
    let mut auto = use_signal(settings);
    rsx! {
        section { class: "pharos-settings-page pharos-offline-page",
            h2 { "On this device" }
            p { class: "pharos-settings-lede",
                "Books kept to read with no connection. They are removed when you sign out."
            }
            h3 { "Keeping books automatically" }
            p { class: "pharos-setting-help",
                "These settings are for this device only. Turning one off removes the books it kept the next time the list or a book opens."
            }
            AutoKeepSettings {
                settings: auto(),
                on_change: move |next: AutoKeep| {
                    save_settings(&next);
                    auto.set(next);
                },
            }
            h3 { "Books on this device" }
            KeptList { store: store(), on_remove, failed: failed() }
        }
    }
}

/// The app with no connection: the kept books, which open without the server.
#[component]
pub fn OfflineHome() -> Element {
    let (store, on_remove, failed) = use_store();
    rsx! { OfflineView { store: store(), on_remove, failed: failed() } }
}
