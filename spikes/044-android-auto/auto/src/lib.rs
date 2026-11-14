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
    /// Tone frequency for playable entries.
    hz: u32,
}

const CATALOGUE: &[Entry] = &[
    Entry { id: "music", parent: ROOT, title: "Music", subtitle: "", playable: false, hz: 0 },
    Entry { id: "audiobooks", parent: ROOT, title: "Audiobooks", subtitle: "", playable: false, hz: 0 },
    Entry { id: "tone:440", parent: "music", title: "Test tone A", subtitle: "Spike 044", playable: true, hz: 440 },
    Entry { id: "tone:523", parent: "music", title: "Test tone C", subtitle: "Spike 044", playable: true, hz: 523 },
    Entry { id: "tone:330", parent: "audiobooks", title: "Test chapter 1", subtitle: "Spike 044", playable: true, hz: 330 },
];

struct State {
    service: GlobalRef,
    session: GlobalRef,
    player: Option<GlobalRef>,
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

fn set_metadata(env: &mut JNIEnv, session: &JObject, e: &Entry) -> R<()> {
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
    env.call_method(&b, "putLong", format!("(Ljava/lang/String;J){B}"), &[JValue::Object(&k), JValue::Long(10_000)])
        .map_err(jerr("putLong"))?;
    let md = env
        .call_method(&b, "build", "()Landroid/media/MediaMetadata;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("MediaMetadata.build"))?;
    env.call_method(session, "setMetadata", "(Landroid/media/MediaMetadata;)V", &[JValue::Object(&md)])
        .map_err(jerr("setMetadata"))?;
    Ok(())
}

/// Plays catalogue entry `id`: writes its tone to the cache dir and hands
/// the file to a fresh `MediaPlayer`.
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

    let player = env.new_object("android/media/MediaPlayer", "()V", &[]).map_err(jerr("MediaPlayer"))?;
    let p = JObject::from(env.new_string(&path).map_err(jerr("path"))?);
    env.call_method(&player, "setDataSource", "(Ljava/lang/String;)V", &[JValue::Object(&p)])
        .map_err(jerr("setDataSource"))?;
    // A local file prepares in milliseconds; a stream will need prepareAsync
    // and a listener (an interface, so a runtime proxy can implement it).
    env.call_method(&player, "prepare", "()V", &[]).map_err(jerr("prepare"))?;
    env.call_method(&player, "start", "()V", &[]).map_err(jerr("start"))?;
    st.player = Some(env.new_global_ref(&player).map_err(jerr("global player"))?);
    st.current = Some(entry.id);
    let session = st.session.clone();
    drop(guard);
    set_metadata(env, session.as_obj(), entry)?;
    set_playback_state(env, session.as_obj(), STATE_PLAYING, 0)?;
    log(&format!("playing id={} hz={} path={path}", entry.id, entry.hz));
    Ok(())
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
        let r = env
            .new_object(
                "android/service/media/MediaBrowserService$BrowserRoot",
                "(Ljava/lang/String;Landroid/os/Bundle;)V",
                &[JValue::Object(&root), JValue::Object(&JObject::null())],
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
    let has_player = state_lock()?.as_ref().is_some_and(|s| s.player.is_some());
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
