//! `PharosMediaService`: create and destroy, the browse root, and the
//! library connection.

use super::*;

/// Used when the app has stored no `https://` server (`credentials::SERVER_FILE`).
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

/// The application id (`Context.getPackageName`).
pub(crate) fn package_name(env: &mut JNIEnv, ctx: &JObject) -> R<String> {
    let name = env
        .call_method(ctx, "getPackageName", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getPackageName"))?;
    string(env, &JString::from(name))
}

/// Starts the library worker once per process. No network I/O happens here.
pub(crate) fn connect_library(env: &mut JNIEnv, ctx: &JObject) -> R<()> {
    // Held across the check and the connect, so the service and the app
    // starting together connect once.
    static CONNECTING: Mutex<()> = Mutex::new(());
    let _connecting = CONNECTING
        .lock()
        .map_err(|_| "library connect mutex poisoned".to_owned())?;
    if LIBRARY.get().is_some() {
        return Ok(());
    }
    if ::log::set_logger(&CONTENT_LOG).is_ok() {
        ::log::set_max_level(::log::LevelFilter::Info);
    }
    let files = files_dir(env, ctx)?;
    let stored = content::credentials::load_server(Path::new(&files));
    let server_from = if stored.is_some() { "file" } else { "default" };
    let server = stored.unwrap_or_else(|| DEFAULT_SERVER.to_owned());
    let lib = content::Library::connect(
        &server,
        &content::credentials::token_path(Path::new(&files)),
    )
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
        runtime(env, &this)?;
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
            started: false,
            from_start: false,
            current: None,
            queue: Vec::new(),
        });
        STATE_SET.notify_all();
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
        // A play still on a worker must not build a player for a dead service.
        supersede();
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

/// `Process.SYSTEM_UID`.
const SYSTEM_UID: i32 = 1000;
/// `ApplicationInfo.FLAG_SYSTEM`.
const FLAG_SYSTEM: i32 = 1;

/// What the platform says about a media-browser client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caller {
    pub uid: i32,
    pub own_uid: i32,
    /// The claimed package is one of the calling uid's packages.
    pub owns_package: bool,
    /// That package is on the system image (Android Auto, SystemUI's media
    /// controls, the Assistant); a sideload cannot replace one.
    pub system_app: bool,
}

/// Whether `caller` may browse the signed-in library: `Ok(reason)` or
/// `Err(reason)`. A package name alone proves nothing, so it must belong to
/// the calling uid and be a system app.
pub(crate) fn access(caller: &Caller) -> Result<&'static str, &'static str> {
    if caller.uid == caller.own_uid {
        return Ok("own_app");
    }
    if caller.uid == SYSTEM_UID || caller.uid == 0 {
        return Ok("system_uid");
    }
    if !caller.owns_package {
        return Err("package_not_of_uid");
    }
    if caller.system_app {
        Ok("system_app")
    } else {
        Err("not_system_app")
    }
}

/// Clears a pending Java exception left by a failed call, so the next JNI
/// call is legal.
fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

fn caller(env: &mut JNIEnv, ctx: &JObject, pkg: &str, uid: i32) -> R<Caller> {
    let own_uid = env
        .call_static_method("android/os/Process", "myUid", "()I", &[])
        .and_then(|v| v.i())
        .map_err(jerr("Process.myUid"))?;
    let pm = env
        .call_method(
            ctx,
            "getPackageManager",
            "()Landroid/content/pm/PackageManager;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getPackageManager"))?;
    let owned = match env
        .call_method(
            &pm,
            "getPackagesForUid",
            "(I)[Ljava/lang/String;",
            &[JValue::Int(uid)],
        )
        .and_then(|v| v.l())
    {
        Ok(a) => a,
        Err(e) => {
            clear_exception(env);
            return Err(format!("getPackagesForUid({uid}): {e}"));
        }
    };
    let mut owns_package = false;
    if !owned.is_null() {
        let owned = jni::objects::JObjectArray::from(owned);
        let n = env
            .get_array_length(&owned)
            .map_err(jerr("packages length"))?;
        for i in 0..n {
            let name = env
                .get_object_array_element(&owned, i)
                .map_err(jerr("packages element"))?;
            if !name.is_null() && string(env, &JString::from(name))? == pkg {
                owns_package = true;
                break;
            }
        }
    }
    let mut system_app = false;
    if owns_package {
        let name = JObject::from(env.new_string(pkg).map_err(jerr("package name"))?);
        match env
            .call_method(
                &pm,
                "getApplicationInfo",
                "(Ljava/lang/String;I)Landroid/content/pm/ApplicationInfo;",
                &[JValue::Object(&name), JValue::Int(0)],
            )
            .and_then(|v| v.l())
        {
            Ok(info) => {
                let flags = env
                    .get_field(&info, "flags", "I")
                    .and_then(|v| v.i())
                    .map_err(jerr("ApplicationInfo.flags"))?;
                system_app = flags & FLAG_SYSTEM != 0;
            }
            // NameNotFoundException: the package went away; not a system app.
            Err(_) => clear_exception(env),
        }
    }
    Ok(Caller {
        uid,
        own_uid,
        owns_package,
        system_app,
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_onGetRoot<'l>(
    mut env: JNIEnv<'l>,
    this: JObject<'l>,
    package: JString<'l>,
    uid: i32,
    _hints: JObject<'l>,
) -> jobject {
    guarded(&mut env, "onGetRoot", std::ptr::null_mut(), |env| {
        let pkg = string(env, &package)?;
        let caller = caller(env, &this, &pkg, uid)?;
        let verdict = access(&caller);
        log(&format!(
            "onGetRoot package={pkg} uid={uid} own_uid={} owns_package={} system_app={} verdict={} reason={}",
            caller.own_uid,
            caller.owns_package,
            caller.system_app,
            if verdict.is_ok() { "allow" } else { "deny" },
            verdict.unwrap_or_else(|reason| reason),
        ));
        if verdict.is_err() {
            return Ok(std::ptr::null_mut());
        }
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

#[cfg(test)]
mod tests {
    use super::{access, Caller};

    fn caller(uid: i32, owns_package: bool, system_app: bool) -> Caller {
        Caller {
            uid,
            own_uid: 10500,
            owns_package,
            system_app,
        }
    }

    #[test]
    fn this_app_and_the_system_may_browse() {
        assert_eq!(access(&caller(10500, false, false)), Ok("own_app"));
        assert_eq!(access(&caller(1000, false, false)), Ok("system_uid"));
    }

    #[test]
    fn a_system_app_whose_package_is_its_own_may_browse() {
        assert_eq!(access(&caller(10123, true, true)), Ok("system_app"));
    }

    #[test]
    fn an_installed_app_may_not_browse_the_library() {
        assert_eq!(access(&caller(10777, true, false)), Err("not_system_app"));
    }

    #[test]
    fn a_package_name_the_caller_does_not_own_proves_nothing() {
        assert_eq!(
            access(&caller(10777, false, true)),
            Err("package_not_of_uid")
        );
    }
}
