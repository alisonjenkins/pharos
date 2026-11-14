//! Spike 044: everything behind the generated `PharosMediaService` and
//! `PharosSessionCallback` classes (see `../classgen`). No Java or Kotlin
//! source exists; each Java entry point lands in one of these functions.
//!
//! The browse tree is the user's Pharos library (`content::Library`) with a
//! "Test" folder of generated tones beside it. Library calls block on the
//! network, so they run on worker threads attached to the JVM, and browse
//! results are detached and sent from there. Every call logs its inputs so a
//! logcat line proves each hop reached Rust; logs carry ids, never titles.

mod art;
mod compat;
mod focus;
mod debug;
mod network;
mod voice;
pub mod content;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jlong, jobject};
use jni::{JNIEnv, JavaVM};

const STATE_STOPPED: i32 = 1;
const STATE_PAUSED: i32 = 2;
const STATE_PLAYING: i32 = 3;
const STATE_ERROR: i32 = 7;
const STATE_BUFFERING: i32 = 6;
// PlaybackState.ACTION_*: STOP | PAUSE | PLAY | SKIP_TO_PREVIOUS |
// SKIP_TO_NEXT | SEEK_TO | PLAY_PAUSE | PLAY_FROM_MEDIA_ID | PLAY_FROM_SEARCH.
const ACTIONS: i64 = 1 | 2 | 4 | 16 | 32 | 256 | 512 | 1024 | 2048 | 16384 | 32768 | 65536;
const FLAG_BROWSABLE: i32 = 1;
const FLAG_PLAYABLE: i32 = 2;

const ROOT: &str = "root";
const TEST: &str = "test";

/// Used when `<filesDir>/server_url` is absent or empty.
const DEFAULT_SERVER: &str = "https://prototype.aliflix.alison-jenkins.com";

/// Browse lists kept so a played track can skip through its siblings.
const REMEMBERED_LISTS: usize = 32;

/// One entry in the fixed test catalogue.
struct Entry {
    id: &'static str,
    parent: &'static str,
    title: &'static str,
    subtitle: &'static str,
    playable: bool,
    /// Tone frequency for playable entries without a `url`.
    hz: u32,
    /// An http(s) stream; when set it plays instead of the tone.
    url: &'static str,
    /// An http(s) image shown as the item's icon and album art.
    art: &'static str,
}

const CATALOGUE: &[Entry] = &[
    Entry { id: TEST, parent: ROOT, title: "Test", subtitle: "Spike 044", playable: false, hz: 0, url: "", art: "" },
    Entry { id: "tone:440", parent: TEST, title: "Test tone A", subtitle: "Spike 044", playable: true, hz: 440, url: "", art: "" },
    Entry { id: "tone:523", parent: TEST, title: "Test tone C", subtitle: "Spike 044", playable: true, hz: 523, url: "", art: "" },
    // A public-domain clip on Wikimedia Commons, to prove streaming.
    Entry {
        id: "stream:example",
        parent: TEST,
        title: "Test stream",
        subtitle: "Spike 044",
        playable: true,
        hz: 0,
        url: "https://upload.wikimedia.org/wikipedia/commons/c/c8/Example.ogg",
        art: "https://upload.wikimedia.org/wikipedia/commons/a/a9/Example.jpg",
    },
    Entry { id: "tone:330", parent: TEST, title: "Test chapter 1", subtitle: "Spike 044", playable: true, hz: 330, url: "", art: "" },
];

/// One row the car shows: a test entry, a library node, or a notice.
#[derive(Debug, Clone)]
struct Item {
    id: String,
    title: String,
    subtitle: String,
    browsable: bool,
    playable: bool,
    /// An http(s) image; empty for none.
    art: String,
    /// 0 when unknown.
    duration_ms: i64,
}

impl Item {
    fn of_entry(e: &Entry) -> Self {
        Self {
            id: e.id.to_owned(),
            title: e.title.to_owned(),
            subtitle: e.subtitle.to_owned(),
            browsable: !e.playable,
            playable: e.playable,
            art: e.art.to_owned(),
            duration_ms: if e.playable && e.url.is_empty() { TONE_MS } else { 0 },
        }
    }

    fn of_node(n: content::Node) -> Self {
        Self {
            id: n.id,
            title: n.title,
            subtitle: n.subtitle,
            browsable: n.browsable,
            playable: n.playable,
            art: n.art_url.unwrap_or_default(),
            duration_ms: i64::try_from(n.duration_ms).unwrap_or(0),
        }
    }

    /// A row that is neither browsable nor playable: it only tells the
    /// driver something (a failure, the library being offline).
    fn notice(title: &str, why: &str) -> Self {
        Self {
            id: "notice".into(),
            title: title.to_owned(),
            subtitle: why.to_owned(),
            browsable: false,
            playable: false,
            art: String::new(),
            duration_ms: 0,
        }
    }
}

struct State {
    service: GlobalRef,
    session: GlobalRef,
    player: Option<GlobalRef>,
    /// False while `player` is still preparing; transport calls wait for it.
    prepared: bool,
    current: Option<Item>,
    /// What skip next/previous moves through; holds `current`.
    queue: Vec<Item>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

static VM: OnceLock<JavaVM> = OnceLock::new();
static LIBRARY: OnceLock<content::Library> = OnceLock::new();

/// App classes, looked up on the main thread: a thread attached from Rust
/// finds classes through the system loader, which does not see the app's.
struct Classes {
    listener: GlobalRef,
    compat_item: GlobalRef,
}
static CLASSES: OnceLock<Classes> = OnceLock::new();

/// Bumped by every play request; a library play that finishes resolving
/// after a newer request is dropped instead of replacing it.
static PLAY_GEN: AtomicU64 = AtomicU64::new(0);

/// Recent browse and search lists' playable rows, newest last.
static LISTS: Mutex<Vec<Vec<Item>>> = Mutex::new(Vec::new());

fn log(msg: &str) {
    #[cfg(target_os = "android")]
    {
        #[link(name = "log")]
        extern "C" {
            fn __android_log_write(prio: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32;
        }
        let Ok(text) = std::ffi::CString::new(msg.replace('\0', " ")) else { return };
        // 4 = ANDROID_LOG_INFO.
        // SAFETY: both strings are NUL-terminated and live across the call.
        unsafe {
            __android_log_write(4, c"pharos-auto".as_ptr(), text.as_ptr());
        }
    }
    #[cfg(not(target_os = "android"))]
    eprintln!("pharos-auto: {msg}");
}

type R<T> = Result<T, String>;

fn jerr(what: &str) -> impl Fn(jni::errors::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Runs `f`; on error logs it with `context`, describes and clears any Java
/// exception so the call returns to Java cleanly.
fn guarded<T>(env: &mut JNIEnv, context: &str, fallback: T, f: impl FnOnce(&mut JNIEnv) -> R<T>) -> T {
    match f(env) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("{context} failed: {e}"));
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            fallback
        }
    }
}

fn string(env: &mut JNIEnv, s: &JString) -> R<String> {
    if s.is_null() {
        return Ok(String::new());
    }
    env.get_string(s).map(Into::into).map_err(jerr("get_string"))
}

fn state_lock() -> R<std::sync::MutexGuard<'static, Option<State>>> {
    STATE.lock().map_err(|_| "state mutex poisoned".to_owned())
}

fn description<'l>(env: &mut JNIEnv<'l>, e: &Item) -> R<JObject<'l>> {
    let b = env
        .new_object("android/media/MediaDescription$Builder", "()V", &[])
        .map_err(jerr("MediaDescription.Builder"))?;
    let id = JObject::from(env.new_string(&e.id).map_err(jerr("id"))?);
    let title = JObject::from(env.new_string(&e.title).map_err(jerr("title"))?);
    let sub = JObject::from(env.new_string(&e.subtitle).map_err(jerr("subtitle"))?);
    const B: &str = "Landroid/media/MediaDescription$Builder;";
    env.call_method(&b, "setMediaId", format!("(Ljava/lang/String;){B}"), &[JValue::Object(&id)])
        .map_err(jerr("setMediaId"))?;
    env.call_method(&b, "setTitle", format!("(Ljava/lang/CharSequence;){B}"), &[JValue::Object(&title)])
        .map_err(jerr("setTitle"))?;
    env.call_method(&b, "setSubtitle", format!("(Ljava/lang/CharSequence;){B}"), &[JValue::Object(&sub)])
        .map_err(jerr("setSubtitle"))?;
    if !e.art.is_empty() {
        let icon = art::art_uri_object(env, &e.art)?;
        env.call_method(&b, "setIconUri", format!("(Landroid/net/Uri;){B}"), &[JValue::Object(&icon)])
            .map_err(jerr("setIconUri"))?;
    }
    env.call_method(&b, "build", "()Landroid/media/MediaDescription;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("MediaDescription.build"))
}

/// A `MediaItem`: the framework's, or (`compat`) the androidx-named
/// subclass the compat search reply needs.
fn media_item<'l>(env: &mut JNIEnv<'l>, e: &Item, compat: bool) -> R<JObject<'l>> {
    let desc = description(env, e)?;
    let mut flags = 0;
    if e.browsable {
        flags |= FLAG_BROWSABLE;
    }
    if e.playable {
        flags |= FLAG_PLAYABLE;
    }
    const SIG: &str = "(Landroid/media/MediaDescription;I)V";
    let args = [JValue::Object(&desc), JValue::Int(flags)];
    if compat {
        let classes = CLASSES.get().ok_or("compat item before onCreate")?;
        let class: &JClass = classes.compat_item.as_obj().into();
        env.new_object(class, SIG, &args).map_err(jerr("compat MediaItem"))
    } else {
        env.new_object("android/media/browse/MediaBrowser$MediaItem", SIG, &args).map_err(jerr("MediaItem"))
    }
}

fn item_list<'l>(env: &mut JNIEnv<'l>, entries: &[Item]) -> R<JObject<'l>> {
    let list = env.new_object("java/util/ArrayList", "()V", &[]).map_err(jerr("ArrayList"))?;
    for e in entries {
        let item = media_item(env, e, false)?;
        env.call_method(&list, "add", "(Ljava/lang/Object;)Z", &[JValue::Object(&item)])
            .map_err(jerr("ArrayList.add"))?;
    }
    Ok(list)
}

fn test_entry(id: &str) -> Option<&'static Entry> {
    CATALOGUE.iter().find(|e| e.id == id)
}

fn test_playables() -> Vec<Item> {
    CATALOGUE.iter().filter(|e| e.playable).map(Item::of_entry).collect()
}

/// Test entries whose title holds `query`; none for an empty query.
fn test_matches(query: &str) -> Vec<Item> {
    let q = query.to_lowercase();
    if q.trim().is_empty() {
        return Vec::new();
    }
    CATALOGUE
        .iter()
        .filter(|e| e.playable && e.title.to_lowercase().contains(&q))
        .map(Item::of_entry)
        .collect()
}

/// What `parent` holds. Blocks on the network for a library node.
fn children(parent: &str) -> Vec<Item> {
    let mut kids: Vec<Item> = if parent != ROOT && test_entry(parent).is_some() {
        CATALOGUE.iter().filter(|e| e.parent == parent).map(Item::of_entry).collect()
    } else {
        match LIBRARY.get() {
            None => vec![Item::notice("Pharos is not connected", "See the app's logs")],
            Some(lib) => match lib.children(parent) {
                Ok(nodes) => nodes.into_iter().map(Item::of_node).collect(),
                Err(e) => {
                    log(&format!("browse {parent} failed: {e}"));
                    vec![Item::notice("Couldn't load this list", &e.to_string())]
                }
            },
        }
    };
    if parent == ROOT {
        kids.extend(CATALOGUE.iter().filter(|e| e.parent == ROOT).map(Item::of_entry));
    }
    kids
}

/// Test matches, then the library's. Blocks on the network.
fn find(query: &str) -> Vec<Item> {
    find_ranked(query).0
}

/// [`find`], with the library's ranking when it searched.
fn find_ranked(query: &str) -> (Vec<Item>, Option<content::SearchReport>) {
    let mut hits = test_matches(query);
    let mut report = None;
    if let Some(lib) = LIBRARY.get() {
        match lib.search_ranked(query) {
            Ok((nodes, r)) => {
                hits.extend(nodes.into_iter().map(Item::of_node));
                report = r;
            }
            Err(e) => {
                log(&format!("library search failed: {e}"));
                if hits.is_empty() {
                    hits.push(Item::notice("Search failed", &e.to_string()));
                }
            }
        }
    }
    (hits, report)
}

/// Plays a search's top result unasked when it clearly beats the rest: a
/// track alone, an album or mix from its first track with the rest queued.
/// Blocks on the network to list an album; run it on a worker.
fn autoplay_search(env: &mut JNIEnv, query_len: usize, hits: &[Item], report: Option<&content::SearchReport>) -> R<()> {
    let (top, second) = report.map_or((0.0, 0.0), |r| (r.top_score, r.second_score));
    let tests = test_matches_count(hits);
    let (mut play_it, mut reason) = match report {
        None => (false, "no_ranking"),
        Some(_) if tests > 0 => (false, "test_matches"),
        Some(r) => content::autoplay(r, hits.len()),
    };
    let mut target: Option<(String, Vec<Item>)> = None;
    if play_it {
        match hits.first() {
            Some(t) if t.playable => target = Some((t.id.clone(), vec![t.clone()])),
            Some(t) if t.browsable => {
                let tracks: Vec<Item> = children(&t.id).into_iter().filter(|i| i.playable).collect();
                match tracks.first() {
                    Some(first) => target = Some((first.id.clone(), tracks)),
                    None => (play_it, reason) = (false, "top_has_no_tracks"),
                }
            }
            _ => (play_it, reason) = (false, "top_not_playable"),
        }
    }
    let top_id = hits.first().map_or("none", |i| i.id.as_str());
    log(&format!(
        "search autoplay q_len={query_len} verdict={} reason={reason} top_score={top:.2} second_score={second:.2} returned={} top_id={top_id}",
        if play_it { "play" } else { "list" },
        hits.len()
    ));
    match target {
        Some((id, queue)) => start(env, &id, Some(queue)),
        None => Ok(()),
    }
}

/// How many of `hits` are test entries rather than library results.
fn test_matches_count(hits: &[Item]) -> usize {
    hits.iter().filter(|i| test_entry(&i.id).is_some()).count()
}

/// Keeps `items`' playable rows so a later play can queue its siblings.
fn remember(items: &[Item]) {
    let playable: Vec<Item> = items.iter().filter(|i| i.playable).cloned().collect();
    if playable.is_empty() {
        return;
    }
    if let Ok(mut lists) = LISTS.lock() {
        lists.retain(|l| l.iter().map(|i| &i.id).ne(playable.iter().map(|i| &i.id)));
        lists.push(playable);
        let excess = lists.len().saturating_sub(REMEMBERED_LISTS);
        lists.drain(..excess);
    }
}

/// The newest remembered list holding `id`, else none.
fn queue_for(id: &str) -> Vec<Item> {
    LISTS
        .lock()
        .ok()
        .and_then(|lists| lists.iter().rev().find(|l| l.iter().any(|i| i.id == id)).cloned())
        .unwrap_or_default()
}

/// Runs `f` on a new thread attached to the JVM.
fn on_worker(name: &str, f: impl FnOnce(&mut JNIEnv) -> R<()> + Send + 'static) -> R<()> {
    let context = name.to_owned();
    std::thread::Builder::new()
        .name("pharos-auto-io".into())
        .spawn(move || {
            let Some(vm) = VM.get() else {
                log(&format!("{context} failed: no JavaVM"));
                return;
            };
            match vm.attach_current_thread() {
                Ok(mut env) => guarded(&mut env, &context, (), f),
                Err(e) => log(&format!("{context} failed: attach: {e}")),
            }
        })
        .map(|_| ())
        .map_err(|e| format!("spawn {name}: {e}"))
}

/// `result.sendResult(items)`; when building the list fails, an empty list,
/// so a detached result is never left unanswered.
fn send_items(env: &mut JNIEnv, result: &JObject, items: &[Item]) -> R<()> {
    let list = match item_list(env, items) {
        Ok(l) => l,
        Err(e) => {
            log(&format!("item list failed, sending an empty one: {e}"));
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
            env.new_object("java/util/ArrayList", "()V", &[]).map_err(jerr("ArrayList"))?
        }
    };
    env.call_method(result, "sendResult", "(Ljava/lang/Object;)V", &[JValue::Object(&list)])
        .map_err(jerr("sendResult"))?;
    Ok(())
}

/// Answers `result` with `work()` from a worker thread.
fn answer_later(env: &mut JNIEnv, name: &str, result: &JObject, work: impl FnOnce() -> Vec<Item> + Send + 'static) -> R<()> {
    env.call_method(result, "detach", "()V", &[]).map_err(jerr("detach"))?;
    let global = env.new_global_ref(result).map_err(jerr("global result"))?;
    let spawned = on_worker(name, move |env| send_items(env, global.as_obj(), &work()));
    if let Err(e) = spawned {
        log(&format!("{name}: {e}; answering now"));
        send_items(env, result, &[Item::notice("Couldn't load this list", &e)])?;
    }
    Ok(())
}

/// Tells the car the root and Music lists changed (sign-in finished).
fn notify_changed() {
    let Some(vm) = VM.get() else { return };
    let Ok(mut env) = vm.attach_current_thread() else { return };
    guarded(&mut env, "notifyChildrenChanged", (), |env| {
        let Some(service) = state_lock()?.as_ref().map(|s| s.service.clone()) else { return Ok(()) };
        for id in [ROOT, "music"] {
            let s = JObject::from(env.new_string(id).map_err(jerr("id"))?);
            env.call_method(service.as_obj(), "notifyChildrenChanged", "(Ljava/lang/String;)V", &[JValue::Object(&s)])
                .map_err(jerr("notifyChildrenChanged"))?;
        }
        log("notifyChildrenChanged root, music");
        Ok(())
    });
}

/// Sends the `log` crate's records (the content layer's) to logcat.
struct ContentLog;

impl ::log::Log for ContentLog {
    fn enabled(&self, m: &::log::Metadata) -> bool {
        m.level() <= ::log::Level::Info
    }
    fn log(&self, r: &::log::Record) {
        if self.enabled(r.metadata()) {
            log(&format!("content {}: {}", r.level(), r.args()));
        }
    }
    fn flush(&self) {}
}

static CONTENT_LOG: ContentLog = ContentLog;

fn files_dir(env: &mut JNIEnv, ctx: &JObject) -> R<String> {
    let dir = env
        .call_method(ctx, "getFilesDir", "()Ljava/io/File;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getFilesDir"))?;
    let dir = env
        .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getAbsolutePath"))?;
    string(env, &JString::from(dir))
}

/// Starts the library worker once per process. No network I/O happens here.
fn connect_library(env: &mut JNIEnv, ctx: &JObject) -> R<()> {
    if LIBRARY.get().is_some() {
        return Ok(());
    }
    if ::log::set_logger(&CONTENT_LOG).is_ok() {
        ::log::set_max_level(::log::LevelFilter::Info);
    }
    let files = files_dir(env, ctx)?;
    let server = std::fs::read_to_string(Path::new(&files).join("server_url"))
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER.to_owned());
    let lib = content::Library::connect(&server, &Path::new(&files).join("device_token"))
        .map_err(|e| format!("library {server}: {e}"))?;
    lib.on_auth_change(|_| notify_changed());
    log(&format!("library server={server} auth={:?}", lib.auth_state()));
    let cache_dir = Path::new(&files).join("audio-cache");
    match lib.enable_cache(&cache_dir, content::DEFAULT_CAP_BYTES) {
        Ok(()) => log(&format!("cache enabled cap_bytes={}", content::DEFAULT_CAP_BYTES)),
        Err(e) => log(&format!("cache disabled: {e}")),
    }
    let _ = LIBRARY.set(lib);
    Ok(())
}

/// A 10 s, 16-bit mono, 22.05 kHz sine WAV at low volume.
fn tone_wav(hz: u32) -> Vec<u8> {
    const RATE: u32 = 22_050;
    let samples = RATE * 10;
    let data_len = samples * 2;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for n in 0..samples {
        let t = f64::from(n) / f64::from(RATE);
        let v = (t * f64::from(hz) * std::f64::consts::TAU).sin() * 0.05 * f64::from(i16::MAX);
        w.extend_from_slice(&(v as i16).to_le_bytes());
    }
    w
}

fn set_playback_state(env: &mut JNIEnv, session: &JObject, state: i32, position_ms: i64) -> R<()> {
    publish_state(env, session, state, position_ms, None)
}

/// ERROR with `message`, which the car shows (and reads out) to the driver.
fn set_error(env: &mut JNIEnv, session: &JObject, message: &str) -> R<()> {
    publish_state(env, session, STATE_ERROR, 0, Some(message))
}

fn publish_state(env: &mut JNIEnv, session: &JObject, state: i32, position_ms: i64, error: Option<&str>) -> R<()> {
    const B: &str = "Landroid/media/session/PlaybackState$Builder;";
    let b = env
        .new_object("android/media/session/PlaybackState$Builder", "()V", &[])
        .map_err(jerr("PlaybackState.Builder"))?;
    let speed = if state == STATE_PLAYING { 1.0f32 } else { 0.0 };
    env.call_method(
        &b,
        "setState",
        format!("(IJF){B}"),
        &[JValue::Int(state), JValue::Long(position_ms), JValue::Float(speed)],
    )
    .map_err(jerr("setState"))?;
    env.call_method(&b, "setActions", format!("(J){B}"), &[JValue::Long(ACTIONS)])
        .map_err(jerr("setActions"))?;
    if let Some(message) = error {
        let m = JObject::from(env.new_string(message).map_err(jerr("error message"))?);
        env.call_method(&b, "setErrorMessage", format!("(Ljava/lang/CharSequence;){B}"), &[JValue::Object(&m)])
            .map_err(jerr("setErrorMessage"))?;
    }
    let ps = env
        .call_method(&b, "build", "()Landroid/media/session/PlaybackState;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("PlaybackState.build"))?;
    env.call_method(session, "setPlaybackState", "(Landroid/media/session/PlaybackState;)V", &[JValue::Object(&ps)])
        .map_err(jerr("setPlaybackState"))?;
    Ok(())
}

fn set_metadata(env: &mut JNIEnv, session: &JObject, e: &Item, duration_ms: i64) -> R<()> {
    const B: &str = "Landroid/media/MediaMetadata$Builder;";
    let b = env
        .new_object("android/media/MediaMetadata$Builder", "()V", &[])
        .map_err(jerr("MediaMetadata.Builder"))?;
    let mut fields = vec![
        ("android.media.metadata.MEDIA_ID", e.id.clone()),
        ("android.media.metadata.TITLE", e.title.clone()),
        ("android.media.metadata.ARTIST", e.subtitle.clone()),
    ];
    if !e.art.is_empty() {
        let uri = art::art_uri(&e.art);
        fields.push(("android.media.metadata.ALBUM_ART_URI", uri.clone()));
        fields.push(("android.media.metadata.DISPLAY_ICON_URI", uri));
    }
    for (key, value) in fields {
        let k = JObject::from(env.new_string(key).map_err(jerr("key"))?);
        let v = JObject::from(env.new_string(value).map_err(jerr("value"))?);
        env.call_method(
            &b,
            "putString",
            format!("(Ljava/lang/String;Ljava/lang/String;){B}"),
            &[JValue::Object(&k), JValue::Object(&v)],
        )
        .map_err(jerr("putString"))?;
    }
    let k = JObject::from(env.new_string("android.media.metadata.DURATION").map_err(jerr("key"))?);
    env.call_method(&b, "putLong", format!("(Ljava/lang/String;J){B}"), &[JValue::Object(&k), JValue::Long(duration_ms)])
        .map_err(jerr("putLong"))?;
    let md = env
        .call_method(&b, "build", "()Landroid/media/MediaMetadata;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("MediaMetadata.build"))?;
    env.call_method(session, "setMetadata", "(Landroid/media/MediaMetadata;)V", &[JValue::Object(&md)])
        .map_err(jerr("setMetadata"))?;
    Ok(())
}

/// Length of every generated tone, in `tone_wav`.
const TONE_MS: i64 = 10_000;

/// Where a test entry plays from: its `url`, else its tone written to the
/// cache dir.
fn test_source(env: &mut JNIEnv, e: &Entry) -> R<String> {
    if !e.url.is_empty() {
        return Ok(e.url.to_owned());
    }
    let service = state_lock()?.as_ref().ok_or("play before onCreate")?.service.clone();
    let dir = env
        .call_method(service.as_obj(), "getCacheDir", "()Ljava/io/File;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getCacheDir"))?;
    let dir = env
        .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getAbsolutePath"))?;
    let dir = string(env, &JString::from(dir))?;
    let path = format!("{dir}/{}.wav", e.id.replace(':', "-"));
    std::fs::write(&path, tone_wav(e.hz)).map_err(|err| format!("write {path}: {err}"))?;
    Ok(path)
}

/// Starts `item` from `source` on a fresh `MediaPlayer`, unless a newer
/// play request than `gen` exists. Preparation is async;
/// `PharosPlayerListener.onPrepared` starts playback.
fn play(env: &mut JNIEnv, item: Item, source: &str, mut queue: Vec<Item>, gen: u64) -> R<()> {
    let mut guard = state_lock()?;
    if PLAY_GEN.load(Ordering::SeqCst) != gen {
        log(&format!("play id={}: a newer request superseded it, dropped", item.id));
        return Ok(());
    }
    let st = guard.as_mut().ok_or("play before onCreate")?;
    if let Some(old) = st.player.take() {
        let _ = env.call_method(old.as_obj(), "release", "()V", &[]);
    }
    st.prepared = false;
    let classes = CLASSES.get().ok_or("play before onCreate")?;
    let player = env.new_object("android/media/MediaPlayer", "()V", &[]).map_err(jerr("MediaPlayer"))?;
    let listener_class: &JClass = classes.listener.as_obj().into();
    let listener = env.new_object(listener_class, "()V", &[]).map_err(jerr("PharosPlayerListener"))?;
    for (setter, iface) in [
        ("setOnPreparedListener", "OnPreparedListener"),
        ("setOnErrorListener", "OnErrorListener"),
        ("setOnCompletionListener", "OnCompletionListener"),
    ] {
        env.call_method(&player, setter, format!("(Landroid/media/MediaPlayer${iface};)V"), &[JValue::Object(&listener)])
            .map_err(jerr(setter))?;
    }
    if let Err(e) = focus::apply(env, &player) {
        log(&format!("play id={}: no audio attributes: {e}", item.id));
    }
    let src = JObject::from(env.new_string(source).map_err(jerr("source"))?);
    env.call_method(&player, "setDataSource", "(Ljava/lang/String;)V", &[JValue::Object(&src)])
        .map_err(jerr("setDataSource"))?;
    env.call_method(&player, "prepareAsync", "()V", &[]).map_err(jerr("prepareAsync"))?;
    st.player = Some(env.new_global_ref(&player).map_err(jerr("global player"))?);
    if !queue.iter().any(|i| i.id == item.id) {
        queue = vec![item.clone()];
    }
    st.current = Some(item.clone());
    st.queue = queue;
    let session = st.session.clone();
    drop(guard);
    set_metadata(env, session.as_obj(), &item, item.duration_ms)?;
    set_playback_state(env, session.as_obj(), STATE_BUFFERING, 0)?;
    log(&format!("preparing id={} source={}", item.id, art::redact(source)));
    Ok(())
}

/// Asks the library for `id`'s fresh stream URL (blocking) and plays it.
fn play_library(env: &mut JNIEnv, id: &str, queue: Vec<Item>, gen: u64) -> R<()> {
    let outcome = (|| {
        let lib = LIBRARY.get().ok_or("library not connected")?;
        if let Some(path) = lib.local_path(id) {
            let listed = queue.iter().find(|i| i.id == id).cloned();
            let item = match listed {
                Some(i) => i,
                None => Item::of_node(lib.node(id).map_err(|e| format!("node {id} (cached, unlisted): {e}"))?),
            };
            let ahead = lib.prefetch_from(id);
            log(&format!("play id={id} verdict=cache-hit prefetch_queued={ahead}"));
            return play(env, item, &path.to_string_lossy(), queue, gen);
        }
        if content::server_unreachable() {
            return Err(format!("node {id}: not cached and the server is unreachable (debug offline)"));
        }
        let node = lib.node(id).map_err(|e| format!("node {id}: {e}"))?;
        let url = node.stream_url.clone().ok_or_else(|| format!("node {id} has no stream"))?;
        let ahead = lib.prefetch_from(id);
        log(&format!("play id={id} verdict=stream prefetch_queued={ahead}"));
        play(env, Item::of_node(node), &url, queue, gen)
    })();
    if let Err(e) = &outcome {
        if PLAY_GEN.load(Ordering::SeqCst) == gen {
            if let Some(session) = state_lock()?.as_ref().map(|s| s.session.clone()) {
                set_error(env, session.as_obj(), "Couldn't play that")?;
            }
        }
        log(&format!("play id={id} failed: {e}"));
    }
    Ok(())
}

/// Plays `id` with `queue` (or the remembered list holding it): a test
/// entry at once, a library node from a worker thread.
fn start(env: &mut JNIEnv, id: &str, queue: Option<Vec<Item>>) -> R<()> {
    let gen = PLAY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(e) = test_entry(id).filter(|e| e.playable) {
        let source = test_source(env, e)?;
        return play(env, Item::of_entry(e), &source, queue.unwrap_or_else(test_playables), gen);
    }
    let id = id.to_owned();
    let queue = queue.unwrap_or_else(|| queue_for(&id));
    on_worker("play", move |env| play_library(env, &id, queue, gen))
}

/// The state's session and current item when `mp` is still the current
/// player; `None` for a player `play` has already replaced.
fn if_current(env: &mut JNIEnv, mp: &JObject) -> R<Option<(GlobalRef, GlobalRef, Item)>> {
    let guard = state_lock()?;
    let Some(st) = guard.as_ref() else { return Ok(None) };
    let Some(player) = &st.player else { return Ok(None) };
    if !env.is_same_object(player.as_obj(), mp).map_err(jerr("is_same_object"))? {
        return Ok(None);
    }
    let item = st.current.clone().ok_or("player without an item")?;
    Ok(Some((st.session.clone(), player.clone(), item)))
}

/// Calls `method` on the current player, then publishes `state`.
fn transport(env: &mut JNIEnv, method: &str, state: i32) -> R<()> {
    let guard = state_lock()?;
    let st = guard.as_ref().ok_or("transport before onCreate")?;
    let session = st.session.clone();
    let Some(player) = st.player.clone() else {
        log(&format!("{method}: nothing loaded, ignored"));
        return Ok(());
    };
    if !st.prepared {
        if method != "stop" {
            log(&format!("{method}: player still preparing, ignored"));
            return Ok(());
        }
        drop(guard);
        if let Some(st) = state_lock()?.as_mut() {
            st.player = None;
        }
        env.call_method(player.as_obj(), "release", "()V", &[]).map_err(jerr("release"))?;
        log("stop: released a player still preparing");
        return set_playback_state(env, session.as_obj(), state, 0);
    }
    drop(guard);
    if method == "start" && !focus::request(env)? {
        log("start: audio focus refused, not started");
        return Ok(());
    }
    env.call_method(player.as_obj(), method, "()V", &[]).map_err(jerr(method))?;
    if method == "stop" {
        focus::abandon(env)?;
    }
    let pos = env
        .call_method(player.as_obj(), "getCurrentPosition", "()I", &[])
        .and_then(|v| v.i())
        .map_err(jerr("getCurrentPosition"))?;
    set_playback_state(env, session.as_obj(), state, i64::from(pos))
}

/// Moves `step` through the queue; past either end it wraps when `wrap`,
/// and stops otherwise. An empty queue plays the first test entry.
fn skip(env: &mut JNIEnv, step: isize, wrap: bool) -> R<()> {
    let (current, queue) = {
        let guard = state_lock()?;
        let st = guard.as_ref().ok_or("skip before onCreate")?;
        (st.current.as_ref().map(|i| i.id.clone()), st.queue.clone())
    };
    if queue.is_empty() {
        let first = test_playables().into_iter().next().ok_or("no test entries")?;
        return start(env, &first.id, None);
    }
    // A track played alone (from search, or offline) still has an album.
    if queue.len() == 1 && step == 1 {
        if let (Some(cur), Some(lib)) = (current.as_deref(), LIBRARY.get()) {
            if let Some(next) = lib.play_queue(cur).into_iter().next() {
                log("skip: next from the library's play queue");
                return start(env, &next, None);
            }
        }
    }
    let at = current.and_then(|c| queue.iter().position(|i| i.id == c)).unwrap_or(0);
    let len = queue.len() as isize;
    let next = at as isize + step;
    if !wrap && !(0..len).contains(&next) {
        log("skip: end of the queue, stopping");
        return transport(env, "stop", STATE_STOPPED);
    }
    let id = queue[next.rem_euclid(len) as usize].id.clone();
    start(env, &id, Some(queue))
}

/// Points the session at `PharosSearchActivity`, the app's only activity,
/// so the system has somewhere to send "open the app".
fn set_session_activity(env: &mut JNIEnv, ctx: &JObject, session: &JObject) -> R<()> {
    const FLAG_IMMUTABLE: i32 = 67_108_864;
    let class = env
        .find_class("com/alisonjenkins/pharos/auto/PharosSearchActivity")
        .map_err(jerr("find PharosSearchActivity"))?;
    let intent = env
        .new_object(
            "android/content/Intent",
            "(Landroid/content/Context;Ljava/lang/Class;)V",
            &[JValue::Object(ctx), JValue::Object(&class)],
        )
        .map_err(jerr("Intent"))?;
    let pi = env
        .call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
            &[JValue::Object(ctx), JValue::Int(0), JValue::Object(&intent), JValue::Int(FLAG_IMMUTABLE)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("PendingIntent.getActivity"))?;
    env.call_method(session, "setSessionActivity", "(Landroid/app/PendingIntent;)V", &[JValue::Object(&pi)])
        .map_err(jerr("setSessionActivity"))?;
    Ok(())
}

/// The keys of `extras`, for logs: which hints (focus, title, artist) the
/// assistant sent with a request.
fn bundle_keys(env: &mut JNIEnv, extras: &JObject) -> R<String> {
    if extras.is_null() {
        return Ok("null".into());
    }
    let keys = env
        .call_method(extras, "keySet", "()Ljava/util/Set;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("keySet"))?;
    let text = env
        .call_method(&keys, "toString", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("keySet.toString"))?;
    string(env, &JString::from(text))
}

/// Loads `item` without starting it, as the PREPARE actions ask: its
/// metadata and PAUSED. A later onPlay plays it.
fn prepare(env: &mut JNIEnv, item: Item, mut queue: Vec<Item>) -> R<()> {
    PLAY_GEN.fetch_add(1, Ordering::SeqCst);
    let session = {
        let mut guard = state_lock()?;
        let st = guard.as_mut().ok_or("prepare before onCreate")?;
        if let Some(old) = st.player.take() {
            let _ = env.call_method(old.as_obj(), "release", "()V", &[]);
        }
        st.prepared = false;
        if !queue.iter().any(|i| i.id == item.id) {
            queue = vec![item.clone()];
        }
        st.current = Some(item.clone());
        st.queue = queue;
        st.session.clone()
    };
    set_metadata(env, session.as_obj(), &item, item.duration_ms)?;
    set_playback_state(env, session.as_obj(), STATE_PAUSED, 0)?;
    log(&format!("prepared id={}", item.id));
    Ok(())
}

/// Prepares `id`: a test entry at once, a library node from a worker.
fn prepare_id(env: &mut JNIEnv, id: &str) -> R<()> {
    if let Some(e) = test_entry(id).filter(|e| e.playable) {
        return prepare(env, Item::of_entry(e), test_playables());
    }
    let id = id.to_owned();
    on_worker("prepare", move |env| {
        let lib = LIBRARY.get().ok_or("library not connected")?;
        let node = lib.node(&id).map_err(|e| format!("node {id}: {e}"))?;
        prepare(env, Item::of_node(node), queue_for(&id))
    })
}

/// Voice "play/prepare X": a test match at once, else the first playable
/// library hit, found on a worker. `then_play` plays it; otherwise it is
/// only prepared.
fn from_search(env: &mut JNIEnv, what: &str, query: String, then_play: bool) -> R<()> {
    if let Some(cmd) = voice::command(&query) {
        log(&format!("{what} verdict={cmd:?} reason=voice_command"));
        return voice::run(env, cmd);
    }
    let tests = test_matches(&query);
    if let Some(first) = tests.first() {
        log(&format!("{what} verdict={} reason=test_match", first.id));
        let id = first.id.clone();
        return if then_play { start(env, &id, Some(test_playables())) } else { prepare_id(env, &id) };
    }
    let gen = PLAY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let what = what.to_owned();
    on_worker("voice search", move |env| {
        let hits = find(&query);
        remember(&hits);
        // A notice (search failed, sign in first) says more than "nothing matched".
        let notice = hits.iter().find(|i| !i.playable && !i.browsable).map(|i| i.title.clone());
        let queue: Vec<Item> = hits.into_iter().filter(|i| i.playable).collect();
        let Some(first) = queue.first().cloned() else {
            log(&format!("{what} verdict=None reason=no_playable_hit"));
            if PLAY_GEN.load(Ordering::SeqCst) == gen {
                if let Some(session) = state_lock()?.as_ref().map(|s| s.session.clone()) {
                    let message = notice.unwrap_or_else(|| format!("Nothing matched {query}"));
                    set_error(env, session.as_obj(), &message)?;
                }
            }
            return Ok(());
        };
        log(&format!("{what} verdict={} reason=library_hit hits={}", first.id, queue.len()));
        if then_play {
            play_library(env, &first.id, queue, gen)
        } else {
            prepare(env, first, queue)
        }
    })
}

// ---- PharosMediaService ----------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_nativeOnCreate<'l>(
    mut env: JNIEnv<'l>,
    this: JObject<'l>,
) {
    guarded(&mut env, "onCreate", (), |env| {
        if VM.get().is_none() {
            let _ = VM.set(env.get_java_vm().map_err(jerr("get_java_vm"))?);
        }
        if CLASSES.get().is_none() {
            let listener = env
                .find_class("com/alisonjenkins/pharos/auto/PharosPlayerListener")
                .map_err(jerr("find PharosPlayerListener"))?;
            let compat_item = env.find_class(compat::ITEM_CLASS).map_err(jerr("find compat MediaItem"))?;
            let _ = CLASSES.set(Classes {
                listener: env.new_global_ref(&listener).map_err(jerr("global listener class"))?,
                compat_item: env.new_global_ref(&compat_item).map_err(jerr("global compat class"))?,
            });
        }
        let tag = JObject::from(env.new_string("pharos").map_err(jerr("tag"))?);
        let session = env
            .new_object(
                "android/media/session/MediaSession",
                "(Landroid/content/Context;Ljava/lang/String;)V",
                &[JValue::Object(&this), JValue::Object(&tag)],
            )
            .map_err(jerr("MediaSession"))?;
        let cb = env
            .new_object("com/alisonjenkins/pharos/auto/PharosSessionCallback", "()V", &[])
            .map_err(jerr("PharosSessionCallback"))?;
        env.call_method(&session, "setCallback", "(Landroid/media/session/MediaSession$Callback;)V", &[JValue::Object(&cb)])
            .map_err(jerr("setCallback"))?;
        set_playback_state(env, &session, STATE_STOPPED, 0)?;
        let listener_class: &JClass = CLASSES.get().ok_or("classes not cached")?.listener.as_obj().into();
        if let Err(e) = focus::create(env, &this, &session, listener_class) {
            log(&format!("onCreate: audio focus unavailable: {e}"));
        }
        // FLAG_HANDLES_MEDIA_BUTTONS | FLAG_HANDLES_TRANSPORT_CONTROLS:
        // deprecated no-ops since API 26, set in case the assistant reads them.
        env.call_method(&session, "setFlags", "(I)V", &[JValue::Int(3)]).map_err(jerr("setFlags"))?;
        set_session_activity(env, &this, &session)?;
        env.call_method(&session, "setActive", "(Z)V", &[JValue::Bool(1)]).map_err(jerr("setActive"))?;
        let token = env
            .call_method(&session, "getSessionToken", "()Landroid/media/session/MediaSession$Token;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getSessionToken"))?;
        env.call_method(&this, "setSessionToken", "(Landroid/media/session/MediaSession$Token;)V", &[JValue::Object(&token)])
            .map_err(jerr("setSessionToken"))?;
        *state_lock()? = Some(State {
            service: env.new_global_ref(&this).map_err(jerr("global service"))?,
            session: env.new_global_ref(&session).map_err(jerr("global session"))?,
            player: None,
            prepared: false,
            current: None,
            queue: Vec::new(),
        });
        log("onCreate: session active, token set");
        // The test folder still works when this fails.
        if let Err(e) = connect_library(env, &this) {
            log(&format!("onCreate: library not connected: {e}"));
        }
        if let Err(e) = network::watch(env, &this) {
            log(&format!("onCreate: network callback unavailable: {e}"));
        }
        if let Err(e) = debug::register(env, &this) {
            log(&format!("onCreate: debug receiver unavailable: {e}"));
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_nativeOnDestroy<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
) {
    guarded(&mut env, "onDestroy", (), |env| {
        let Some(st) = state_lock()?.take() else { return Ok(()) };
        focus::destroy(env)?;
        network::unwatch(env)?;
        debug::unregister(env)?;
        if let Some(p) = st.player {
            env.call_method(p.as_obj(), "release", "()V", &[]).map_err(jerr("player.release"))?;
        }
        env.call_method(st.session.as_obj(), "release", "()V", &[]).map_err(jerr("session.release"))?;
        log("onDestroy: released");
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_onGetRoot<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    package: JString<'l>,
    uid: i32,
    _hints: JObject<'l>,
) -> jobject {
    guarded(&mut env, "onGetRoot", std::ptr::null_mut(), |env| {
        let pkg = string(env, &package)?;
        log(&format!("onGetRoot package={pkg} uid={uid} verdict=allow reason=spike_allows_all"));
        let root = JObject::from(env.new_string(ROOT).map_err(jerr("root"))?);
        // Without this extra the car hides its search button (DHU check,
        // 2026-11-14): voice still binds, but typed search never reaches onSearch.
        let extras = env.new_object("android/os/Bundle", "()V", &[]).map_err(jerr("Bundle"))?;
        let key = JObject::from(env.new_string("android.media.browse.SEARCH_SUPPORTED").map_err(jerr("key"))?);
        env.call_method(&extras, "putBoolean", "(Ljava/lang/String;Z)V", &[JValue::Object(&key), JValue::Bool(1)])
            .map_err(jerr("putBoolean"))?;
        compat::advertise(env, &extras)?;
        let r = env
            .new_object(
                "android/service/media/MediaBrowserService$BrowserRoot",
                "(Ljava/lang/String;Landroid/os/Bundle;)V",
                &[JValue::Object(&root), JValue::Object(&extras)],
            )
            .map_err(jerr("BrowserRoot"))?;
        Ok(r.into_raw())
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_onLoadChildren<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    parent: JString<'l>,
    result: JObject<'l>,
) {
    guarded(&mut env, "onLoadChildren", (), |env| {
        let parent = string(env, &parent)?;
        log(&format!("onLoadChildren parent={parent}"));
        answer_later(env, "onLoadChildren", &result, move || {
            let kids = children(&parent);
            log(&format!("onLoadChildren parent={parent} children={}", kids.len()));
            remember(&kids);
            kids
        })
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_onSearch<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    query: JString<'l>,
    _extras: JObject<'l>,
    result: JObject<'l>,
) {
    guarded(&mut env, "onSearch", (), |env| {
        let q = string(env, &query)?;
        answer_later(env, "onSearch", &result, move || {
            let hits = find(&q);
            log(&format!("onSearch query={q:?} hits={}", hits.len()));
            remember(&hits);
            hits
        })
    })
}

// ---- PharosSessionCallback -------------------------------------------------

macro_rules! callback {
    ($fn:ident, $name:literal, |$env:ident| $body:expr) => {
        #[no_mangle]
        pub extern "system" fn $fn<'l>(mut env: JNIEnv<'l>, _this: JObject<'l>) {
            log(concat!("session ", $name));
            guarded(&mut env, $name, (), |$env| $body)
        }
    };
}

callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlay, "onPlay", |env| {
    let has_player = state_lock()?.as_ref().is_some_and(|s| s.player.is_some() && s.prepared);
    if has_player {
        transport(env, "start", STATE_PLAYING)
    } else {
        skip(env, 0, true)
    }
});
callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPause, "onPause", |env| {
    transport(env, "pause", STATE_PAUSED)
});
callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onStop, "onStop", |env| {
    transport(env, "stop", STATE_STOPPED)
});
callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSkipToNext, "onSkipToNext", |env| {
    skip(env, 1, true)
});
callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSkipToPrevious, "onSkipToPrevious", |env| {
    skip(env, -1, true)
});

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSeekTo<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    pos: jlong,
) {
    guarded(&mut env, "onSeekTo", (), |env| {
        log(&format!("session onSeekTo pos_ms={pos}"));
        let guard = state_lock()?;
        let Some(st) = guard.as_ref() else { return Ok(()) };
        let (Some(player), session) = (st.player.clone(), st.session.clone()) else { return Ok(()) };
        if !st.prepared {
            log("onSeekTo: player still preparing, ignored");
            return Ok(());
        }
        drop(guard);
        let ms = i32::try_from(pos).unwrap_or(i32::MAX);
        env.call_method(player.as_obj(), "seekTo", "(I)V", &[JValue::Int(ms)]).map_err(jerr("seekTo"))?;
        // A seek while paused stays paused: the car's progress bar runs only at PLAYING.
        let playing = env.call_method(player.as_obj(), "isPlaying", "()Z", &[]).and_then(|v| v.z()).map_err(jerr("isPlaying"))?;
        let state = if playing { STATE_PLAYING } else { STATE_PAUSED };
        log(&format!("onSeekTo pos_ms={pos} state={state}"));
        set_playback_state(env, session.as_obj(), state, pos)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlayFromMediaId<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    id: JString<'l>,
    _extras: JObject<'l>,
) {
    guarded(&mut env, "onPlayFromMediaId", (), |env| {
        let id = string(env, &id)?;
        log(&format!("session onPlayFromMediaId id={id}"));
        start(env, &id, None)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlayFromSearch<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    query: JString<'l>,
    extras: JObject<'l>,
) {
    guarded(&mut env, "onPlayFromSearch", (), |env| {
        let q = string(env, &query)?;
        let keys = bundle_keys(env, &extras)?;
        log(&format!("session onPlayFromSearch query={q:?} extras={keys}"));
        from_search(env, "onPlayFromSearch", q, true)
    })
}

callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepare, "onPrepare", |env| {
    let current = state_lock()?.as_ref().and_then(|s| s.current.clone());
    match current {
        Some(item) => {
            let queue = state_lock()?.as_ref().map(|s| s.queue.clone()).unwrap_or_default();
            prepare(env, item, queue)
        }
        None => match test_playables().into_iter().next() {
            Some(first) => prepare(env, first, test_playables()),
            None => Ok(()),
        },
    }
});

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepareFromMediaId<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    id: JString<'l>,
    _extras: JObject<'l>,
) {
    guarded(&mut env, "onPrepareFromMediaId", (), |env| {
        let id = string(env, &id)?;
        log(&format!("session onPrepareFromMediaId id={id}"));
        prepare_id(env, &id)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepareFromSearch<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    query: JString<'l>,
    extras: JObject<'l>,
) {
    guarded(&mut env, "onPrepareFromSearch", (), |env| {
        let q = string(env, &query)?;
        let keys = bundle_keys(env, &extras)?;
        log(&format!("session onPrepareFromSearch query={q:?} extras={keys}"));
        from_search(env, "onPrepareFromSearch", q, false)
    })
}

// ---- PharosPlayerListener -------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onPrepared<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
) {
    guarded(&mut env, "onPrepared", (), |env| {
        let Some((session, player, entry)) = if_current(env, &mp)? else {
            log("player onPrepared: stale player, not started");
            return Ok(());
        };
        let duration = env
            .call_method(player.as_obj(), "getDuration", "()I", &[])
            .and_then(|v| v.i())
            .map_err(jerr("getDuration"))?;
        if let Some(st) = state_lock()?.as_mut() {
            st.prepared = true;
        }
        // getDuration is -1 for a live stream: fall back to the server's length.
        let length = if duration > 0 { i64::from(duration) } else { entry.duration_ms };
        let source = if duration > 0 { "player" } else { "server" };
        log(&format!(
            "duration source={source} ms={length} player_ms={duration} server_ms={} id={}",
            entry.duration_ms, entry.id
        ));
        set_metadata(env, session.as_obj(), &entry, length)?;
        if !focus::request(env)? {
            set_playback_state(env, session.as_obj(), STATE_PAUSED, 0)?;
            log(&format!("player onPrepared id={}: audio focus refused, paused", entry.id));
            return Ok(());
        }
        env.call_method(player.as_obj(), "start", "()V", &[]).map_err(jerr("start"))?;
        set_playback_state(env, session.as_obj(), STATE_PLAYING, 0)?;
        log(&format!("player onPrepared id={} duration_ms={length}: started", entry.id));
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onError<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
    what: jni::sys::jint,
    extra: jni::sys::jint,
) -> jni::sys::jboolean {
    guarded(&mut env, "onError", (), |env| {
        let Some((session, _, entry)) = if_current(env, &mp)? else {
            log(&format!("player onError what={what} extra={extra}: stale player, ignored"));
            return Ok(());
        };
        if let Some(st) = state_lock()?.as_mut() {
            st.player = None;
            st.prepared = false;
        }
        let _ = env.call_method(&mp, "release", "()V", &[]);
        set_playback_state(env, session.as_obj(), STATE_ERROR, 0)?;
        log(&format!("player onError id={} what={what} extra={extra}: released, state ERROR", entry.id));
        Ok(())
    });
    // Handled: without this MediaPlayer also calls onCompletion.
    1
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onCompletion<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
) {
    guarded(&mut env, "onCompletion", (), |env| {
        let Some((_, _, entry)) = if_current(env, &mp)? else {
            log("player onCompletion: stale player, ignored");
            return Ok(());
        };
        log(&format!("player onCompletion id={}: next in the queue", entry.id));
        skip(env, 1, false)
    })
}

// ---- PharosSearchActivity --------------------------------------------------

/// The assistant's "play X on <app>" from the phone side arrives as a
/// MEDIA_PLAY_FROM_SEARCH intent to this invisible activity. It plays the
/// best match if the media service is running, and finishes either way.
#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSearchActivity_nativeOnCreate<'l>(
    mut env: JNIEnv<'l>,
    this: JObject<'l>,
    _saved: JObject<'l>,
) {
    guarded(&mut env, "search activity", (), |env| {
        let intent = env
            .call_method(&this, "getIntent", "()Landroid/content/Intent;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getIntent"))?;
        let action = env
            .call_method(&intent, "getAction", "()Ljava/lang/String;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getAction"))?;
        let action = string(env, &JString::from(action))?;
        let key = JObject::from(env.new_string("query").map_err(jerr("key"))?);
        let q = env
            .call_method(&intent, "getStringExtra", "(Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&key)])
            .and_then(|v| v.l())
            .map_err(jerr("getStringExtra"))?;
        let q = string(env, &JString::from(q))?;
        let extras = env
            .call_method(&intent, "getExtras", "()Landroid/os/Bundle;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getExtras"))?;
        let keys = bundle_keys(env, &extras)?;
        let service_up = state_lock()?.is_some();
        let is_search = action == "android.media.action.MEDIA_PLAY_FROM_SEARCH";
        log(&format!("search activity action={action} query={q:?} extras={keys} service_up={service_up}"));
        // The session activity opens it without a search: show nothing, play nothing.
        let played = if service_up && is_search { from_search(env, "search activity", q, true) } else { Ok(()) };
        env.call_method(&this, "finish", "()V", &[]).map_err(jerr("finish"))?;
        played
    })
}
