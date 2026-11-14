//! `PharosMediaService`: create and destroy, the browse root, and the
//! library connection.

use super::*;

/// Used when `<filesDir>/server_url` is absent or empty.
pub(crate) const DEFAULT_SERVER: &str = "https://prototype.aliflix.alison-jenkins.com";

/// Sends the `log` crate's records (the content layer's) to logcat.
pub(crate) struct ContentLog;

pub(crate) static CONTENT_LOG: ContentLog = ContentLog;

pub(crate) fn files_dir(env: &mut JNIEnv, ctx: &JObject) -> R<String> {
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
pub(crate) fn connect_library(env: &mut JNIEnv, ctx: &JObject) -> R<()> {
    if LIBRARY.get().is_some() {
        return Ok(());
    }
    if ::log::set_logger(&CONTENT_LOG).is_ok() {
        ::log::set_max_level(::log::LevelFilter::Info);
    }
    let files = files_dir(env, ctx)?;
    let stored = std::fs::read_to_string(Path::new(&files).join("server_url"))
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty());
    let server_from = if stored.is_some() { "file" } else { "default" };
    let server = stored.unwrap_or_else(|| DEFAULT_SERVER.to_owned());
    let lib = content::Library::connect(&server, &Path::new(&files).join("device_token"))
        .map_err(|e| format!("library server_from={server_from}: {}", e.redacted()))?;
    lib.on_auth_change(|_| notify_changed());
    let auth = match lib.auth_state() {
        content::AuthState::SignedOut => "signed_out",
        content::AuthState::Pending { .. } => "pending",
        content::AuthState::SignedIn => "signed_in",
    };
    log(&format!("library server_from={server_from} auth={auth}"));
    let cache_dir = Path::new(&files).join("audio-cache");
    match lib.enable_cache(&cache_dir, content::DEFAULT_CAP_BYTES) {
        Ok(()) => log(&format!(
            "cache enabled cap_bytes={}",
            lib.cache_cap().unwrap_or(0)
        )),
        Err(e) => log(&format!("cache disabled: {}", e.redacted())),
    }
    let _ = LIBRARY.set(lib);
    Ok(())
}

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
            let compat_item = env
                .find_class(compat::ITEM_CLASS)
                .map_err(jerr("find compat MediaItem"))?;
            let _ = CLASSES.set(Classes {
                listener: env
                    .new_global_ref(&listener)
                    .map_err(jerr("global listener class"))?,
                compat_item: env
                    .new_global_ref(&compat_item)
                    .map_err(jerr("global compat class"))?,
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
            .new_object(
                "com/alisonjenkins/pharos/auto/PharosSessionCallback",
                "()V",
                &[],
            )
            .map_err(jerr("PharosSessionCallback"))?;
        env.call_method(
            &session,
            "setCallback",
            "(Landroid/media/session/MediaSession$Callback;)V",
            &[JValue::Object(&cb)],
        )
        .map_err(jerr("setCallback"))?;
        set_playback_state(env, &session, STATE_STOPPED, 0)?;
        let listener_class: &JClass = CLASSES
            .get()
            .ok_or("classes not cached")?
            .listener
            .as_obj()
            .into();
        if let Err(e) = focus::create(env, &this, &session, listener_class) {
            log(&format!("onCreate: audio focus unavailable: {e}"));
        }
        // FLAG_HANDLES_MEDIA_BUTTONS | FLAG_HANDLES_TRANSPORT_CONTROLS:
        // deprecated no-ops since API 26, set in case the assistant reads them.
        env.call_method(&session, "setFlags", "(I)V", &[JValue::Int(3)])
            .map_err(jerr("setFlags"))?;
        set_session_activity(env, &this, &session)?;
        env.call_method(&session, "setActive", "(Z)V", &[JValue::Bool(1)])
            .map_err(jerr("setActive"))?;
        let token = env
            .call_method(
                &session,
                "getSessionToken",
                "()Landroid/media/session/MediaSession$Token;",
                &[],
            )
            .and_then(|v| v.l())
            .map_err(jerr("getSessionToken"))?;
        env.call_method(
            &this,
            "setSessionToken",
            "(Landroid/media/session/MediaSession$Token;)V",
            &[JValue::Object(&token)],
        )
        .map_err(jerr("setSessionToken"))?;
        if let Err(e) = foreground::create(env, &this, &token) {
            log(&format!("onCreate: media notification unavailable: {e}"));
        }
        *state_lock()? = Some(State {
            service: env.new_global_ref(&this).map_err(jerr("global service"))?,
            session: env
                .new_global_ref(&session)
                .map_err(jerr("global session"))?,
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
        if let Err(e) = noisy::register(env, &this) {
            log(&format!(
                "onCreate: becoming-noisy receiver unavailable: {e}"
            ));
        }
        if let Err(e) = files_dir(env, &this).and_then(|dir| resume::open(&dir)) {
            log(&format!("onCreate: resume positions unavailable: {e}"));
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
        let Some(st) = state_lock()?.take() else {
            return Ok(());
        };
        focus::destroy(env)?;
        foreground::destroy(env)?;
        network::unwatch(env)?;
        debug::unregister(env)?;
        noisy::unregister(env)?;
        resume::leave(env, "destroy")?;
        resume::close();
        if let Some(p) = st.player {
            env.call_method(p.as_obj(), "release", "()V", &[])
                .map_err(jerr("player.release"))?;
        }
        env.call_method(st.session.as_obj(), "release", "()V", &[])
            .map_err(jerr("session.release"))?;
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
        log(&format!(
            "onGetRoot package={pkg} uid={uid} verdict=allow reason=any_caller"
        ));
        let root = JObject::from(env.new_string(ROOT).map_err(jerr("root"))?);
        // Without this extra the car hides its search button (DHU check,
        // 2026-11-14): voice still binds, but typed search never reaches onSearch.
        let extras = env
            .new_object("android/os/Bundle", "()V", &[])
            .map_err(jerr("Bundle"))?;
        let key = JObject::from(
            env.new_string("android.media.browse.SEARCH_SUPPORTED")
                .map_err(jerr("key"))?,
        );
        env.call_method(
            &extras,
            "putBoolean",
            "(Ljava/lang/String;Z)V",
            &[JValue::Object(&key), JValue::Bool(1)],
        )
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
            log(&format!(
                "onLoadChildren parent={parent} children={}",
                kids.len()
            ));
            remember(&kids);
            kids
        })
    })
}
