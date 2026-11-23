//! `PharosMediaService`: create and destroy, the browse root, and the
//! library connection.

use super::*;
use std::sync::atomic::AtomicBool;

/// Used when the app has stored no `https://` server (`credentials::SERVER_FILE`).
pub(crate) const DEFAULT_SERVER: &str = "https://prototype.aliflix.alison-jenkins.com";

/// Sends the `log` crate's records (the content layer's) to logcat.
pub(crate) struct ContentLog;

pub(crate) static CONTENT_LOG: ContentLog = ContentLog;

/// What other downloads (the phone's offline video) hold of the cache's
/// cap, as the app reports it. The library's hook reads this when it runs,
/// so it can be set after the library is up.
static OUTSIDE_USED: OnceLock<content::OutsideUsed> = OnceLock::new();

/// Registers the app's count of other downloads' bytes against the music
/// cache's cap. The cache counts them as used and evicts only unpinned
/// music for them: pinned music is never evicted, so it can take the total
/// over the one cap (the pin contract). The hook runs under
/// the cache's lock and must not call back into the library. Set once;
/// later calls are ignored.
pub fn set_outside_used(used: content::OutsideUsed) {
    let _ = OUTSIDE_USED.set(used);
}

/// What the app runs, with the files directory, when the library is
/// connected: it registers the video accounting (`set_outside_used` and the
/// video store). The app's own start-up does not run in a process the car
/// started with no activity, so the app fills this slot as its library
/// loads. Set once. It answers whether it is done: `false` when the video
/// store could not be opened (its folder or its thread), so a later connect
/// or prefetch runs it again (`retry_init`). Music stays refused meanwhile:
/// the accounting reads unknown while no store is registered.
static INIT_HOOK: OnceLock<fn(&str) -> bool> = OnceLock::new();

pub fn set_init_hook(hook: fn(&str) -> bool) {
    let _ = INIT_HOOK.set(hook);
}

/// The files directory the library was connected with, for `retry_init`.
static FILES: OnceLock<String> = OnceLock::new();
/// Whether `INIT_HOOK` has said it is done.
static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// Runs the init hook again unless it is done, so a service-only process
/// whose video folder failed at connect registers the store once the folder
/// can be made, with no activity to do it. `when` names the caller in the
/// log line.
pub(crate) fn retry_init(when: &str) -> InitRetry {
    let verdict = rerun_init(
        &INIT_DONE,
        FILES.get().map(String::as_str),
        INIT_HOOK.get().copied(),
    );
    if verdict != InitRetry::AlreadyDone {
        log(&format!(
            "video accounting retry when={when} verdict={}",
            verdict.label()
        ));
    }
    // The video accounting now counts: a cache that opened before it did
    // has not yet trimmed with it. Owed once, settled by whichever of this
    // retry and `connect_library` sees both the accounting and the library.
    if verdict == InitRetry::Done {
        TRIM_OWED.store(true, Ordering::SeqCst);
    }
    settle_trim(when);
    verdict
}

/// A trim the accounting's arrival owes the music cache, until one path
/// has settled it.
static TRIM_OWED: AtomicBool = AtomicBool::new(false);

/// What settling the owed trim found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrimVerdict {
    /// Nothing owed (never arrived, or another path already settled it).
    NotOwed,
    /// Owed, but the library is not published yet: it stays owed for the
    /// path that publishes it.
    NoLibraryYet,
    /// Owed, the cache is not enabled: a cache enabled later trims as it
    /// opens, so the debt is dropped.
    CacheNotOpen,
    /// Owed and due now. Returned to exactly one caller.
    Trim,
}

/// Settles the owed trim: the one atomic swap that takes it decides who
/// trims, so the accounting arriving before or after the library publishes
/// trims once.
pub(crate) fn take_trim(owed: &AtomicBool, library: bool, cache_enabled: bool) -> TrimVerdict {
    if !library {
        return if owed.load(Ordering::SeqCst) {
            TrimVerdict::NoLibraryYet
        } else {
            TrimVerdict::NotOwed
        };
    }
    if !owed.swap(false, Ordering::SeqCst) {
        return TrimVerdict::NotOwed;
    }
    if cache_enabled {
        TrimVerdict::Trim
    } else {
        TrimVerdict::CacheNotOpen
    }
}

/// Runs the owed trim if the library is published; `by` names the path.
fn settle_trim(by: &str) {
    let lib = LIBRARY.get();
    let verdict = take_trim(
        &TRIM_OWED,
        lib.is_some(),
        lib.is_some_and(|l| l.cache_enabled()),
    );
    if verdict == TrimVerdict::NotOwed {
        return;
    }
    if let (TrimVerdict::Trim, Some(lib)) = (verdict, lib) {
        lib.set_outside_used(std::sync::Arc::new(|| outside_bytes(OUTSIDE_USED.get())));
    }
    log(&format!(
        "video accounting trim by={by} verdict={verdict:?} reason=accounting_installed_after_cache"
    ));
}

/// How long the background retry waits between tries, and how many it
/// makes in one run (about ten minutes): a later connect or prefetch starts
/// another run.
const INIT_RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
const INIT_RETRY_TRIES: u32 = 20;

/// Calls `step` until it reports the init done, waiting `every` between
/// calls (`sleep`), at most `tries` times. Returns the number of calls.
/// Writes stay refused meanwhile: the accounting reads unknown until the
/// store registers.
pub(crate) fn retry_until_done(
    tries: u32,
    mut step: impl FnMut() -> InitRetry,
    mut sleep: impl FnMut(),
) -> u32 {
    for made in 1..=tries {
        if matches!(step(), InitRetry::Done | InitRetry::AlreadyDone) {
            return made;
        }
        if made < tries {
            sleep();
        }
    }
    tries
}

/// Retries the init on a thread of its own until it is done, even when no
/// playback ever starts, sharing the single-flight guard with
/// [`retry_init_in_background`].
pub(crate) fn schedule_init_retries(when: &'static str) {
    if INIT_DONE.load(Ordering::Acquire) {
        return;
    }
    let verdict = spawn_once(&RETRY_RUNNING, move || {
        let made = retry_until_done(
            INIT_RETRY_TRIES,
            || retry_init(when),
            || std::thread::sleep(INIT_RETRY_EVERY),
        );
        log(&format!(
            "video accounting retry when={when} loop=ended tries={made} done={}",
            INIT_DONE.load(Ordering::Acquire)
        ));
    });
    if verdict != Background::Started {
        log(&format!(
            "video accounting retry when={when} loop={}",
            verdict.label()
        ));
    }
}

/// Whether a background `retry_init` is running now.
static RETRY_RUNNING: AtomicBool = AtomicBool::new(false);

/// `retry_init` on a thread of its own, at most one at a time, for a caller
/// that must not wait: opening the video store walks its folder, and the
/// playback path would hold the track for it. Music stays refused while it
/// runs, as the accounting reads not installed until it is done.
pub(crate) fn retry_init_in_background(when: &'static str) {
    if INIT_DONE.load(Ordering::Acquire) {
        return;
    }
    let verdict = spawn_once(&RETRY_RUNNING, move || {
        retry_init(when);
    });
    if verdict != Background::Started {
        log(&format!(
            "video accounting retry when={when} verdict={}",
            verdict.label()
        ));
    }
}

/// What `spawn_once` did.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Background {
    /// The work runs on a new thread.
    Started,
    /// One already runs: not started again.
    AlreadyRunning,
    /// No thread could be made: not run, and a later caller tries again.
    SpawnFailed,
}

impl Background {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::AlreadyRunning => "already_running",
            Self::SpawnFailed => "spawn_failed",
        }
    }
}

/// Runs `work` on a thread of its own unless `running` says one runs, and
/// clears `running` when it ends, however it ends.
pub(crate) fn spawn_once(
    running: &'static AtomicBool,
    work: impl FnOnce() + Send + 'static,
) -> Background {
    if running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Background::AlreadyRunning;
    }
    struct Clear(&'static AtomicBool);
    impl Drop for Clear {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let spawned = std::thread::Builder::new()
        .name("pharos-video-init".to_owned())
        .spawn(move || {
            let _clear = Clear(running);
            work();
        });
    if spawned.is_ok() {
        Background::Started
    } else {
        running.store(false, Ordering::Release);
        Background::SpawnFailed
    }
}

/// What `rerun_init` did.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum InitRetry {
    /// The hook said it was done before: not run.
    AlreadyDone,
    /// No hook, or no library connected yet: nothing to run.
    NothingToRun,
    /// Run, and done now.
    Done,
    /// Run, and still not done: a later caller tries again.
    StillMissing,
}

impl InitRetry {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::AlreadyDone => "already_done",
            Self::NothingToRun => "nothing_to_run",
            Self::Done => "done",
            Self::StillMissing => "still_missing",
        }
    }
}

/// Runs `hook` over `files` unless `done` says it is done, and records its
/// answer.
pub(crate) fn rerun_init(
    done: &AtomicBool,
    files: Option<&str>,
    hook: Option<fn(&str) -> bool>,
) -> InitRetry {
    if done.load(Ordering::Acquire) {
        return InitRetry::AlreadyDone;
    }
    let (Some(files), Some(hook)) = (files, hook) else {
        return InitRetry::NothingToRun;
    };
    if hook(files) {
        done.store(true, Ordering::Release);
        InitRetry::Done
    } else {
        InitRetry::StillMissing
    }
}

/// Whether the video's bytes are counted against the music cache. Music
/// is not prefetched before they are: it would spend room an admitted
/// video needs.
pub(crate) fn accounting_installed() -> bool {
    OUTSIDE_USED.get().is_some()
}

/// What the library's hook reports. With no accounting installed nothing is
/// known of what the app's downloads hold, so the size is unknown and the
/// music cache admits nothing, as prefetch (`prefetch_refusal`) does; it is
/// not read as zero usage.
pub(crate) fn outside_bytes(used: Option<&content::OutsideUsed>) -> content::OutsideBytes {
    used.map_or(
        content::OutsideBytes {
            unknown: true,
            ..content::OutsideBytes::default()
        },
        |used| used(),
    )
}

/// Why prefetch must not run, when it must not.
pub(crate) fn prefetch_refusal(installed: bool) -> Option<&'static str> {
    (!installed).then_some("video accounting is not installed")
}

/// The app's private files directory (`Context.getFilesDir`).
pub fn files_dir(env: &mut Env, ctx: &JObject) -> R<String> {
    let dir = env
        .call_method(
            ctx,
            jni::jni_str!("getFilesDir"),
            jni::jni_sig!("()Ljava/io/File;"),
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getFilesDir"))?;
    let dir = env
        .call_method(
            &dir,
            jni::jni_str!("getAbsolutePath"),
            jni::jni_sig!("()Ljava/lang/String;"),
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getAbsolutePath"))?;
    string(env, &dir)
}

/// The application id (`Context.getPackageName`).
pub(crate) fn package_name(env: &mut Env, ctx: &JObject) -> R<String> {
    let name = env
        .call_method(
            ctx,
            jni::jni_str!("getPackageName"),
            jni::jni_sig!("()Ljava/lang/String;"),
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getPackageName"))?;
    string(env, &name)
}

/// Starts the library worker once per process. No network I/O happens here.
pub(crate) fn connect_library(env: &mut Env, ctx: &JObject) -> R<()> {
    // Held across the check and the connect, so the service and the app
    // starting together connect once.
    static CONNECTING: Mutex<()> = Mutex::new(());
    let _connecting = CONNECTING
        .lock()
        .map_err(|_| "library connect mutex poisoned".to_owned())?;
    if let Some(lib) = LIBRARY.get() {
        retry_init("connect");
        // A cache that could not be enabled at the first connect (its
        // folder, say) is tried again here; an enabled one is left alone.
        if let Some(files) = FILES.get() {
            ensure_cache(lib, files, "connect");
        }
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
    // Before the cache is enabled or anything prefetches: the app's video
    // accounting, in a process that never ran the app's own start-up.
    let _ = FILES.set(files.clone());
    // Run again in the background while the first try fails, so a car-only
    // process that never plays still registers its video store.
    if retry_init("first_connect") == InitRetry::StillMissing {
        schedule_init_retries("first_connect");
    }
    lib.on_auth_change(|_| notify_changed());
    let auth = match lib.auth_state() {
        content::AuthState::SignedOut => "signed_out",
        content::AuthState::Pending { .. } => "pending",
        content::AuthState::SignedIn => "signed_in",
    };
    log(&format!("library server_from={server_from} auth={auth}"));
    lib.set_outside_used(std::sync::Arc::new(|| outside_bytes(OUTSIDE_USED.get())));
    ensure_cache(&lib, &files, "first_connect");
    let _ = LIBRARY.set(lib);
    // A retry that finished the init while the library was unpublished left
    // the trim owed.
    settle_trim("connect");
    Ok(())
}

/// What [`ensure_cache`] found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheSetup {
    AlreadyEnabled,
    Enabled,
    Failed,
}

/// Enables the music cache under `files` unless it is already, logging the
/// verdict (`when` names the caller).
pub(crate) fn ensure_cache(lib: &content::Library, files: &str, when: &str) -> CacheSetup {
    if lib.cache_enabled() {
        return CacheSetup::AlreadyEnabled;
    }
    let cache_dir = Path::new(files).join("audio-cache");
    match lib.enable_cache(&cache_dir, content::DEFAULT_CAP_BYTES) {
        Ok(()) => {
            log(&format!(
                "cache enabled when={when} cap_bytes={}",
                lib.cache_cap().unwrap_or(0)
            ));
            CacheSetup::Enabled
        }
        Err(e) => {
            log(&format!(
                "cache disabled when={when} retry=next_connect: {}",
                e.redacted()
            ));
            CacheSetup::Failed
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_nativeOnCreate<'l>(
    mut env: EnvUnowned<'l>,
    this: JObject<'l>,
) {
    guarded_native(&mut env, "onCreate", (), |env| {
        runtime(env, &this)?;
        let tag = JObject::from(env.new_string("pharos").map_err(jerr("tag"))?);
        let session = env
            .new_object(
                jni::jni_str!("android/media/session/MediaSession"),
                jni::jni_sig!("(Landroid/content/Context;Ljava/lang/String;)V"),
                &[JValue::Object(&this), JValue::Object(&tag)],
            )
            .map_err(jerr("MediaSession"))?;
        let cb = env
            .new_object(
                jni::jni_str!("com/alisonjenkins/pharos/auto/PharosSessionCallback"),
                jni::jni_sig!("()V"),
                &[],
            )
            .map_err(jerr("PharosSessionCallback"))?;
        env.call_method(
            &session,
            jni::jni_str!("setCallback"),
            jni::jni_sig!("(Landroid/media/session/MediaSession$Callback;)V"),
            &[JValue::Object(&cb)],
        )
        .map_err(jerr("setCallback"))?;
        set_playback_state(env, &session, STATE_STOPPED, 0)?;
        let listener_class = &CLASSES.get().ok_or("classes not cached")?.listener;
        if let Err(e) = focus::create(env, &this, &session, listener_class) {
            log(&format!("onCreate: audio focus unavailable: {e}"));
        }
        // FLAG_HANDLES_MEDIA_BUTTONS | FLAG_HANDLES_TRANSPORT_CONTROLS:
        // deprecated no-ops since API 26, set in case the assistant reads them.
        env.call_method(
            &session,
            jni::jni_str!("setFlags"),
            jni::jni_sig!("(I)V"),
            &[JValue::Int(3)],
        )
        .map_err(jerr("setFlags"))?;
        set_session_activity(env, &this, &session)?;
        env.call_method(
            &session,
            jni::jni_str!("setActive"),
            jni::jni_sig!("(Z)V"),
            &[JValue::Bool(true)],
        )
        .map_err(jerr("setActive"))?;
        let token = env
            .call_method(
                &session,
                jni::jni_str!("getSessionToken"),
                jni::jni_sig!("()Landroid/media/session/MediaSession$Token;"),
                &[],
            )
            .and_then(|v| v.l())
            .map_err(jerr("getSessionToken"))?;
        env.call_method(
            &this,
            jni::jni_str!("setSessionToken"),
            jni::jni_sig!("(Landroid/media/session/MediaSession$Token;)V"),
            &[JValue::Object(&token)],
        )
        .map_err(jerr("setSessionToken"))?;
        if let Err(e) = foreground::create(env, &this, &token) {
            log(&format!("onCreate: media notification unavailable: {e}"));
        }
        *state_lock()? = Some(State {
            service: GlobalRef::new(env, &this).map_err(jerr("global service"))?,
            session: GlobalRef::new(env, &session).map_err(jerr("global session"))?,
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
    mut env: EnvUnowned<'l>,
    _this: JObject<'l>,
) {
    guarded_native(&mut env, "onDestroy", (), |env| {
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
            env.call_method(
                p.as_obj(),
                jni::jni_str!("release"),
                jni::jni_sig!("()V"),
                &[],
            )
            .map_err(jerr("player.release"))?;
        }
        env.call_method(
            st.session.as_obj(),
            jni::jni_str!("release"),
            jni::jni_sig!("()V"),
            &[],
        )
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
fn clear_exception(env: &mut Env) {
    if env.exception_check() {
        env.exception_clear();
    }
}

fn caller(env: &mut Env, ctx: &JObject, pkg: &str, uid: i32) -> R<Caller> {
    let own_uid = env
        .call_static_method(
            jni::jni_str!("android/os/Process"),
            jni::jni_str!("myUid"),
            jni::jni_sig!("()I"),
            &[],
        )
        .and_then(|v| v.i())
        .map_err(jerr("Process.myUid"))?;
    let pm = env
        .call_method(
            ctx,
            jni::jni_str!("getPackageManager"),
            jni::jni_sig!("()Landroid/content/pm/PackageManager;"),
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getPackageManager"))?;
    let owned = match env
        .call_method(
            &pm,
            jni::jni_str!("getPackagesForUid"),
            jni::jni_sig!("(I)[Ljava/lang/String;"),
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
        let owned = env
            .cast_local::<jni::objects::JObjectArray<JObject>>(owned)
            .map_err(jerr("packages array"))?;
        let n = owned.len(env).map_err(jerr("packages length"))?;
        for i in 0..n {
            let name = owned
                .get_element(env, i)
                .map_err(jerr("packages element"))?;
            if !name.is_null() && string(env, &name)? == pkg {
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
                jni::jni_str!("getApplicationInfo"),
                jni::jni_sig!("(Ljava/lang/String;I)Landroid/content/pm/ApplicationInfo;"),
                &[JValue::Object(&name), JValue::Int(0)],
            )
            .and_then(|v| v.l())
        {
            Ok(info) => {
                let flags = env
                    .get_field(&info, jni::jni_str!("flags"), jni::jni_sig!("I"))
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
    mut env: EnvUnowned<'l>,
    this: JObject<'l>,
    package: JString<'l>,
    uid: i32,
    _hints: JObject<'l>,
) -> jobject {
    guarded_native(&mut env, "onGetRoot", std::ptr::null_mut(), |env| {
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
            .new_object(
                jni::jni_str!("android/os/Bundle"),
                jni::jni_sig!("()V"),
                &[],
            )
            .map_err(jerr("Bundle"))?;
        let key = JObject::from(
            env.new_string("android.media.browse.SEARCH_SUPPORTED")
                .map_err(jerr("key"))?,
        );
        env.call_method(
            &extras,
            jni::jni_str!("putBoolean"),
            jni::jni_sig!("(Ljava/lang/String;Z)V"),
            &[JValue::Object(&key), JValue::Bool(true)],
        )
        .map_err(jerr("putBoolean"))?;
        compat::advertise(env, &extras)?;
        let r = env
            .new_object(
                jni::jni_str!("android/service/media/MediaBrowserService$BrowserRoot"),
                jni::jni_sig!("(Ljava/lang/String;Landroid/os/Bundle;)V"),
                &[JValue::Object(&root), JValue::Object(&extras)],
            )
            .map_err(jerr("BrowserRoot"))?;
        Ok(r.into_raw())
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_onLoadChildren<'l>(
    mut env: EnvUnowned<'l>,
    _this: JObject<'l>,
    parent: JString<'l>,
    result: JObject<'l>,
) {
    guarded_native(&mut env, "onLoadChildren", (), |env| {
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
    use super::{
        access, outside_bytes, rerun_init, retry_until_done, spawn_once, take_trim, Background,
        Caller, InitRetry, TrimVerdict,
    };
    use pharos_content as content;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// CR review 6018104500 (service.rs:54-58): the retry that finishes the
    /// init trims an already-enabled cache once; later retries (AlreadyDone)
    /// and a cache not yet enabled do not.
    #[test]
    fn only_the_first_successful_retry_trims_an_enabled_cache() {
        let owed = AtomicBool::new(false);
        // Only Done owes a trim; the other verdicts leave nothing owed.
        assert_eq!(take_trim(&owed, true, true), TrimVerdict::NotOwed);
        owed.store(true, Ordering::SeqCst);
        assert_eq!(take_trim(&owed, true, true), TrimVerdict::Trim);
        assert_eq!(take_trim(&owed, true, true), TrimVerdict::NotOwed);
        // A cache not yet enabled trims as it opens: the debt is dropped.
        owed.store(true, Ordering::SeqCst);
        assert_eq!(take_trim(&owed, true, false), TrimVerdict::CacheNotOpen);
        assert_eq!(take_trim(&owed, true, true), TrimVerdict::NotOwed);
    }

    /// CR (service.rs:58): the retry that finishes the init may run before
    /// or after `connect_library` publishes the library; the trim runs once
    /// either way.
    #[test]
    fn the_trim_runs_once_whichever_of_the_retry_and_the_publish_comes_first() {
        let trims = |order: &[&str]| {
            let owed = AtomicBool::new(false);
            let mut library = false;
            let mut n = 0;
            for step in order {
                match *step {
                    "retry_done" => owed.store(true, Ordering::SeqCst),
                    "publish" => library = true,
                    _ => unreachable!(),
                }
                // Each step ends with the settle both paths run.
                if take_trim(&owed, library, true) == TrimVerdict::Trim {
                    n += 1;
                }
            }
            // A later connect or retry settles again: nothing more.
            if take_trim(&owed, library, true) == TrimVerdict::Trim {
                n += 1;
            }
            n
        };
        assert_eq!(trims(&["retry_done", "publish"]), 1, "retry first");
        assert_eq!(trims(&["publish", "retry_done"]), 1, "publish first");
    }

    /// CR review 6018104500 (service.rs:280): the background retry keeps
    /// trying at an interval until the init is done, and stops there; it is
    /// bounded when it never is.
    #[test]
    fn the_background_retry_runs_until_done_and_is_bounded() {
        let mut answers = vec![
            InitRetry::StillMissing,
            InitRetry::StillMissing,
            InitRetry::Done,
        ]
        .into_iter();
        let (mut sleeps, mut calls) = (0, 0);
        let made = retry_until_done(
            20,
            || {
                calls += 1;
                answers.next().unwrap_or(InitRetry::Done)
            },
            || sleeps += 1,
        );
        assert_eq!((made, calls, sleeps), (3, 3, 2), "it did not stop at done");
        let (mut calls, mut sleeps) = (0, 0);
        let made = retry_until_done(
            4,
            || {
                calls += 1;
                InitRetry::StillMissing
            },
            || sleeps += 1,
        );
        assert_eq!((made, calls, sleeps), (4, 4, 3), "it was not bounded");
    }

    /// The playback path's retry returns at once while the store opens, and
    /// a second caller meanwhile does not start another open; once it ends
    /// the next caller can.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_background_retry_does_not_hold_the_caller_and_runs_once_at_a_time() {
        static RUNNING: AtomicBool = AtomicBool::new(false);
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (done, ended) = std::sync::mpsc::channel::<()>();
        // The caller runs on a thread of its own: the open it starts waits
        // for `release`, sent only after the caller returned, so a caller
        // that waited for the open would never return. The bounded wait is
        // a deadlock guard, not a latency bound (CR 4644634426).
        let (returned, caller) = std::sync::mpsc::channel::<Background>();
        std::thread::spawn(move || {
            let verdict = spawn_once(&RUNNING, move || {
                wait.recv().unwrap();
                done.send(()).unwrap();
            });
            returned.send(verdict).unwrap();
        });
        let first = caller
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("the caller waited for the open: {e}"));
        assert_eq!(first, Background::Started);
        assert!(
            ended.try_recv().is_err(),
            "the open ended before it was released"
        );
        assert_eq!(
            spawn_once(&RUNNING, || unreachable!("a second open ran")),
            Background::AlreadyRunning
        );
        release.send(()).unwrap();
        ended.recv().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while RUNNING.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "never cleared");
            std::thread::yield_now();
        }
        let (again, ran) = std::sync::mpsc::channel::<()>();
        assert_eq!(
            spawn_once(&RUNNING, move || again.send(()).unwrap()),
            Background::Started
        );
        ran.recv().unwrap();
        assert_ne!(
            Background::Started.label(),
            Background::AlreadyRunning.label()
        );
        assert_ne!(
            Background::AlreadyRunning.label(),
            Background::SpawnFailed.label()
        );
    }

    /// A music cache that could not be enabled is tried again by the next
    /// connect, and an enabled one is left alone.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_cache_that_failed_to_enable_is_tried_again() {
        use super::{ensure_cache, CacheSetup};
        /// Removes the fixture folder when the test ends, a panic included.
        /// Declared before the library, so the library drops first.
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        // Created exclusively: a name already taken (a leftover, another
        // process) is skipped, never reused or deleted, and only the folder
        // this test made is removed.
        let dir = (0u32..100)
            .find_map(|n| {
                let dir = std::env::temp_dir().join(format!(
                    "pharos-android-cache-retry-{}-{n}",
                    std::process::id()
                ));
                std::fs::create_dir(&dir).ok().map(|()| dir)
            })
            .unwrap_or_else(|| panic!("no free fixture folder name"));
        let _fixture = Fixture(dir.clone());
        let files = dir.to_str().unwrap().to_owned();
        let lib = content::Library::connect("http://127.0.0.1:9", &dir.join("token")).unwrap();
        // A file where the cache's folder should be.
        std::fs::write(dir.join("audio-cache"), b"x").unwrap();
        assert_eq!(
            ensure_cache(&lib, &files, "first_connect"),
            CacheSetup::Failed
        );
        std::fs::remove_file(dir.join("audio-cache")).unwrap();
        assert_eq!(ensure_cache(&lib, &files, "connect"), CacheSetup::Enabled);
        assert!(lib.cache_enabled());
        assert_eq!(
            ensure_cache(&lib, &files, "connect"),
            CacheSetup::AlreadyEnabled
        );
    }

    /// A hook that failed is run again by the next caller, and one that
    /// said it is done is not run again.
    #[test]
    fn an_init_hook_that_is_not_done_is_run_again_until_it_is() {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        fn hook(files: &str) -> bool {
            assert_eq!(files, "/files");
            RUNS.fetch_add(1, Ordering::SeqCst) >= 1
        }
        let done = AtomicBool::new(false);
        assert_eq!(rerun_init(&done, None, Some(hook)), InitRetry::NothingToRun);
        assert_eq!(
            rerun_init(&done, Some("/files"), None),
            InitRetry::NothingToRun
        );
        assert_eq!(
            rerun_init(&done, Some("/files"), Some(hook)),
            InitRetry::StillMissing
        );
        assert_eq!(
            rerun_init(&done, Some("/files"), Some(hook)),
            InitRetry::Done
        );
        assert_eq!(
            rerun_init(&done, Some("/files"), Some(hook)),
            InitRetry::AlreadyDone
        );
        assert_eq!(RUNS.load(Ordering::SeqCst), 2, "a done hook ran again");
    }

    #[test]
    fn no_installed_accounting_reads_as_unknown_not_zero() {
        assert!(outside_bytes(None).unknown);
        let used: content::OutsideUsed = std::sync::Arc::new(|| content::OutsideBytes {
            held: 5,
            outstanding: 2,
            unknown: false,
        });
        let o = outside_bytes(Some(&used));
        assert_eq!((o.held, o.outstanding, o.unknown), (5, 2, false));
    }

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
