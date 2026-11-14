//! Spike 044: everything behind the generated `PharosMediaService` and
//! `PharosSessionCallback` classes (see `../classgen`). No Java or Kotlin
//! source exists; each Java entry point lands in one of these functions.
//!
//! The spike shows a two-level browse tree (Music, Audiobooks), plays a
//! generated test tone through `android.media.MediaPlayer`, answers voice
//! search (`onSearch`, `onPlayFromSearch`), and logs every call with its
//! inputs so a logcat line proves each hop reached Rust.

use std::sync::Mutex;

use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::sys::{jlong, jobject};
use jni::JNIEnv;

const STATE_STOPPED: i32 = 1;
const STATE_PAUSED: i32 = 2;
const STATE_PLAYING: i32 = 3;
const STATE_ERROR: i32 = 7;
const STATE_BUFFERING: i32 = 6;
// PlaybackState.ACTION_*: STOP | PAUSE | PLAY | SKIP_TO_PREVIOUS |
// SKIP_TO_NEXT | SEEK_TO | PLAY_PAUSE | PLAY_FROM_MEDIA_ID | PLAY_FROM_SEARCH.
const ACTIONS: i64 = 1 | 2 | 4 | 16 | 32 | 256 | 512 | 1024 | 2048;
const FLAG_BROWSABLE: i32 = 1;
const FLAG_PLAYABLE: i32 = 2;

const ROOT: &str = "root";

/// One entry in the spike's fixed catalogue.
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
}

const CATALOGUE: &[Entry] = &[
    Entry { id: "music", parent: ROOT, title: "Music", subtitle: "", playable: false, hz: 0, url: "" },
    Entry { id: "audiobooks", parent: ROOT, title: "Audiobooks", subtitle: "", playable: false, hz: 0, url: "" },
    Entry { id: "tone:440", parent: "music", title: "Test tone A", subtitle: "Spike 044", playable: true, hz: 440, url: "" },
    Entry { id: "tone:523", parent: "music", title: "Test tone C", subtitle: "Spike 044", playable: true, hz: 523, url: "" },
    // A public-domain clip on Wikimedia Commons, to prove streaming.
    Entry {
        id: "stream:example",
        parent: "music",
        title: "Test stream",
        subtitle: "Spike 044",
        playable: true,
        hz: 0,
        url: "https://upload.wikimedia.org/wikipedia/commons/c/c8/Example.ogg",
    },
    Entry { id: "tone:330", parent: "audiobooks", title: "Test chapter 1", subtitle: "Spike 044", playable: true, hz: 330, url: "" },
];

struct State {
    service: GlobalRef,
    session: GlobalRef,
    player: Option<GlobalRef>,
    /// False while `player` is still preparing; transport calls wait for it.
    prepared: bool,
    current: Option<&'static str>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

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

fn media_item<'l>(env: &mut JNIEnv<'l>, e: &Entry) -> R<JObject<'l>> {
    let b = env
        .new_object("android/media/MediaDescription$Builder", "()V", &[])
        .map_err(jerr("MediaDescription.Builder"))?;
    let id = JObject::from(env.new_string(e.id).map_err(jerr("id"))?);
    let title = JObject::from(env.new_string(e.title).map_err(jerr("title"))?);
    let sub = JObject::from(env.new_string(e.subtitle).map_err(jerr("subtitle"))?);
    const B: &str = "Landroid/media/MediaDescription$Builder;";
    env.call_method(&b, "setMediaId", format!("(Ljava/lang/String;){B}"), &[JValue::Object(&id)])
        .map_err(jerr("setMediaId"))?;
    env.call_method(&b, "setTitle", format!("(Ljava/lang/CharSequence;){B}"), &[JValue::Object(&title)])
        .map_err(jerr("setTitle"))?;
    env.call_method(&b, "setSubtitle", format!("(Ljava/lang/CharSequence;){B}"), &[JValue::Object(&sub)])
        .map_err(jerr("setSubtitle"))?;
    let desc = env
        .call_method(&b, "build", "()Landroid/media/MediaDescription;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("MediaDescription.build"))?;
    let flags = if e.playable { FLAG_PLAYABLE } else { FLAG_BROWSABLE };
    env.new_object(
        "android/media/browse/MediaBrowser$MediaItem",
        "(Landroid/media/MediaDescription;I)V",
        &[JValue::Object(&desc), JValue::Int(flags)],
    )
    .map_err(jerr("MediaItem"))
}

fn item_list<'l>(env: &mut JNIEnv<'l>, entries: &[&Entry]) -> R<JObject<'l>> {
    let list = env.new_object("java/util/ArrayList", "()V", &[]).map_err(jerr("ArrayList"))?;
    for e in entries {
        let item = media_item(env, e)?;
        env.call_method(&list, "add", "(Ljava/lang/Object;)Z", &[JValue::Object(&item)])
            .map_err(jerr("ArrayList.add"))?;
    }
    Ok(list)
}

fn search(query: &str) -> Vec<&'static Entry> {
    let q = query.to_lowercase();
    let hits: Vec<_> = CATALOGUE
        .iter()
        .filter(|e| e.playable && (q.is_empty() || e.title.to_lowercase().contains(&q)))
        .collect();
    // An unmatched voice request still plays something, as the Auto
    // guidelines ask ("play music on Pharos").
    if hits.is_empty() {
        CATALOGUE.iter().filter(|e| e.playable).take(1).collect()
    } else {
        hits
    }
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
    let ps = env
        .call_method(&b, "build", "()Landroid/media/session/PlaybackState;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("PlaybackState.build"))?;
    env.call_method(session, "setPlaybackState", "(Landroid/media/session/PlaybackState;)V", &[JValue::Object(&ps)])
        .map_err(jerr("setPlaybackState"))?;
    Ok(())
}

fn set_metadata(env: &mut JNIEnv, session: &JObject, e: &Entry, duration_ms: i64) -> R<()> {
    const B: &str = "Landroid/media/MediaMetadata$Builder;";
    let b = env
        .new_object("android/media/MediaMetadata$Builder", "()V", &[])
        .map_err(jerr("MediaMetadata.Builder"))?;
    for (key, value) in [
        ("android.media.metadata.MEDIA_ID", e.id),
        ("android.media.metadata.TITLE", e.title),
        ("android.media.metadata.ARTIST", e.subtitle),
    ] {
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

/// Starts catalogue entry `id` on a fresh `MediaPlayer`: its `url` if it
/// has one, else its tone written to the cache dir. Preparation is async;
/// `PharosPlayerListener.onPrepared` starts playback.
fn play(env: &mut JNIEnv, id: &str) -> R<()> {
    let entry = CATALOGUE
        .iter()
        .find(|e| e.playable && e.id == id)
        .ok_or_else(|| format!("no playable entry {id:?}"))?;
    let mut guard = state_lock()?;
    let st = guard.as_mut().ok_or("play before onCreate")?;
    if let Some(old) = st.player.take() {
        let _ = env.call_method(old.as_obj(), "release", "()V", &[]);
    }
    st.prepared = false;
    let source = if entry.url.is_empty() {
        let dir = env
            .call_method(st.service.as_obj(), "getCacheDir", "()Ljava/io/File;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getCacheDir"))?;
        let dir = env
            .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getAbsolutePath"))?;
        let dir = string(env, &JString::from(dir))?;
        let path = format!("{dir}/{}.wav", entry.id.replace(':', "-"));
        std::fs::write(&path, tone_wav(entry.hz)).map_err(|e| format!("write {path}: {e}"))?;
        path
    } else {
        entry.url.to_owned()
    };

    let player = env.new_object("android/media/MediaPlayer", "()V", &[]).map_err(jerr("MediaPlayer"))?;
    let listener = env
        .new_object("com/alisonjenkins/pharos/auto/PharosPlayerListener", "()V", &[])
        .map_err(jerr("PharosPlayerListener"))?;
    for (setter, iface) in [
        ("setOnPreparedListener", "OnPreparedListener"),
        ("setOnErrorListener", "OnErrorListener"),
        ("setOnCompletionListener", "OnCompletionListener"),
    ] {
        env.call_method(&player, setter, format!("(Landroid/media/MediaPlayer${iface};)V"), &[JValue::Object(&listener)])
            .map_err(jerr(setter))?;
    }
    let src = JObject::from(env.new_string(&source).map_err(jerr("source"))?);
    env.call_method(&player, "setDataSource", "(Ljava/lang/String;)V", &[JValue::Object(&src)])
        .map_err(jerr("setDataSource"))?;
    env.call_method(&player, "prepareAsync", "()V", &[]).map_err(jerr("prepareAsync"))?;
    st.player = Some(env.new_global_ref(&player).map_err(jerr("global player"))?);
    st.current = Some(entry.id);
    let session = st.session.clone();
    drop(guard);
    let duration = if entry.url.is_empty() { TONE_MS } else { 0 };
    set_metadata(env, session.as_obj(), entry, duration)?;
    set_playback_state(env, session.as_obj(), STATE_BUFFERING, 0)?;
    log(&format!("preparing id={} source={source}", entry.id));
    Ok(())
}

/// The state's session and current entry when `mp` is still the current
/// player; `None` for a player `play` has already replaced.
fn if_current(env: &mut JNIEnv, mp: &JObject) -> R<Option<(GlobalRef, GlobalRef, &'static Entry)>> {
    let guard = state_lock()?;
    let Some(st) = guard.as_ref() else { return Ok(None) };
    let Some(player) = &st.player else { return Ok(None) };
    if !env.is_same_object(player.as_obj(), mp).map_err(jerr("is_same_object"))? {
        return Ok(None);
    }
    let entry = st.current.and_then(|id| CATALOGUE.iter().find(|e| e.id == id)).ok_or("player without an entry")?;
    Ok(Some((st.session.clone(), player.clone(), entry)))
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
    env.call_method(player.as_obj(), method, "()V", &[]).map_err(jerr(method))?;
    let pos = env
        .call_method(player.as_obj(), "getCurrentPosition", "()I", &[])
        .and_then(|v| v.i())
        .map_err(jerr("getCurrentPosition"))?;
    set_playback_state(env, session.as_obj(), state, i64::from(pos))
}

fn skip(env: &mut JNIEnv, step: isize) -> R<()> {
    let current = state_lock()?.as_ref().and_then(|s| s.current);
    let playable: Vec<_> = CATALOGUE.iter().filter(|e| e.playable).collect();
    let at = current.and_then(|c| playable.iter().position(|e| e.id == c)).unwrap_or(0);
    let len = playable.len() as isize;
    let next = (at as isize + step).rem_euclid(len) as usize;
    match playable.get(next) {
        Some(e) => play(env, e.id),
        None => Ok(()),
    }
}

// ---- PharosMediaService ----------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_nativeOnCreate<'l>(
    mut env: JNIEnv<'l>,
    this: JObject<'l>,
) {
    guarded(&mut env, "onCreate", (), |env| {
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
        });
        log("onCreate: session active, token set");
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
        let kids: Vec<_> = CATALOGUE.iter().filter(|e| e.parent == parent).collect();
        log(&format!("onLoadChildren parent={parent} children={}", kids.len()));
        let list = item_list(env, &kids)?;
        env.call_method(&result, "sendResult", "(Ljava/lang/Object;)V", &[JValue::Object(&list)])
            .map_err(jerr("sendResult"))?;
        Ok(())
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
        let hits = search(&q);
        log(&format!("onSearch query={q:?} hits={}", hits.len()));
        let list = item_list(env, &hits)?;
        env.call_method(&result, "sendResult", "(Ljava/lang/Object;)V", &[JValue::Object(&list)])
            .map_err(jerr("sendResult"))?;
        Ok(())
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
        skip(env, 0)
    }
});
callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPause, "onPause", |env| {
    transport(env, "pause", STATE_PAUSED)
});
callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onStop, "onStop", |env| {
    transport(env, "stop", STATE_STOPPED)
});
callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSkipToNext, "onSkipToNext", |env| {
    skip(env, 1)
});
callback!(Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSkipToPrevious, "onSkipToPrevious", |env| {
    skip(env, -1)
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
        set_playback_state(env, session.as_obj(), STATE_PLAYING, pos)
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
        play(env, &id)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlayFromSearch<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    query: JString<'l>,
    _extras: JObject<'l>,
) {
    guarded(&mut env, "onPlayFromSearch", (), |env| {
        let q = string(env, &query)?;
        let hit = search(&q).first().map(|e| e.id);
        log(&format!("session onPlayFromSearch query={q:?} verdict={hit:?}"));
        match hit {
            Some(id) => play(env, id),
            None => Ok(()),
        }
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
        env.call_method(player.as_obj(), "start", "()V", &[]).map_err(jerr("start"))?;
        if let Some(st) = state_lock()?.as_mut() {
            st.prepared = true;
        }
        // getDuration is -1 for a live stream; leave the length unknown.
        set_metadata(env, session.as_obj(), entry, i64::from(duration.max(0)))?;
        set_playback_state(env, session.as_obj(), STATE_PLAYING, 0)?;
        log(&format!("player onPrepared id={} duration_ms={duration}: started", entry.id));
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
        log(&format!("player onCompletion id={}: skipping to next", entry.id));
        skip(env, 1)
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
        let hit = search(&q).first().map(|e| e.id);
        let service_up = state_lock()?.is_some();
        log(&format!(
            "search activity action={action} query={q:?} verdict={hit:?} service_up={service_up}"
        ));
        let played = match (hit, service_up) {
            (Some(id), true) => play(env, id),
            _ => Ok(()),
        };
        env.call_method(&this, "finish", "()V", &[]).map_err(jerr("finish"))?;
        played
    })
}
