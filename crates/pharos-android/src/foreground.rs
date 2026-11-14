//! The media notification and foreground state. Playing holds the service
//! in the foreground, so playback survives a locked phone with no car
//! bound to it; pausing leaves the foreground but keeps the notification;
//! stopping removes it and lets the service stop.
//!
//! A `MediaStyle` notification tied to the session is exempt from the
//! `POST_NOTIFICATIONS` runtime permission (API 33), and on API 33+ the
//! system draws its controls from the session's playback state.

use std::sync::Mutex;

use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;

use crate::{jerr, log, R, STATE_BUFFERING, STATE_PAUSED, STATE_PLAYING};

const CHANNEL_ID: &str = "playback";
const CHANNEL_NAME: &str = "Playback";
/// `NotificationManager.IMPORTANCE_LOW`: shown, but silent.
const IMPORTANCE_LOW: i32 = 2;
const NOTIFICATION_ID: i32 = 1;
/// `ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK`, matching the manifest.
const FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK: i32 = 2;
/// `Service.STOP_FOREGROUND_REMOVE` / `STOP_FOREGROUND_DETACH`.
const STOP_FOREGROUND_REMOVE: i32 = 1;
const STOP_FOREGROUND_DETACH: i32 = 2;
/// `Notification.VISIBILITY_PUBLIC`: the lock screen shows the controls.
const VISIBILITY_PUBLIC: i32 = 1;

struct Fg {
    service: GlobalRef,
    token: GlobalRef,
    manager: GlobalRef,
    title: String,
    text: String,
    /// `startForegroundService` ran, so the service must `stopSelf` on stop.
    started: bool,
    foreground: bool,
}

static FG: Mutex<Option<Fg>> = Mutex::new(None);

fn fg() -> R<std::sync::MutexGuard<'static, Option<Fg>>> {
    FG.lock()
        .map_err(|_| "foreground mutex poisoned".to_owned())
}

/// Creates the notification channel. `token` is the session's token.
pub fn create(env: &mut JNIEnv, service: &JObject, token: &JObject) -> R<()> {
    let name = JObject::from(
        env.new_string("notification")
            .map_err(jerr("service name"))?,
    );
    let manager = env
        .call_method(
            service,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&name)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getSystemService(notification)"))?;
    if manager.is_null() {
        return Err("getSystemService(notification) returned null".into());
    }
    let id = JObject::from(env.new_string(CHANNEL_ID).map_err(jerr("channel id"))?);
    let label = JObject::from(env.new_string(CHANNEL_NAME).map_err(jerr("channel name"))?);
    let channel = env
        .new_object(
            "android/app/NotificationChannel",
            "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
            &[
                JValue::Object(&id),
                JValue::Object(&label),
                JValue::Int(IMPORTANCE_LOW),
            ],
        )
        .map_err(jerr("NotificationChannel"))?;
    env.call_method(
        &manager,
        "createNotificationChannel",
        "(Landroid/app/NotificationChannel;)V",
        &[JValue::Object(&channel)],
    )
    .map_err(jerr("createNotificationChannel"))?;
    *fg()? = Some(Fg {
        service: env
            .new_global_ref(service)
            .map_err(jerr("global fg service"))?,
        token: env.new_global_ref(token).map_err(jerr("global fg token"))?,
        manager: env
            .new_global_ref(&manager)
            .map_err(jerr("global notification manager"))?,
        title: String::new(),
        text: String::new(),
        started: false,
        foreground: false,
    });
    log(&format!("notify: channel created id={CHANNEL_ID}"));
    Ok(())
}

/// The text the next notification shows; set with the session metadata.
pub fn describe(title: &str, text: &str) -> R<()> {
    if let Some(f) = fg()?.as_mut() {
        f.title = title.to_owned();
        f.text = text.to_owned();
    }
    Ok(())
}

/// Follows a published playback state. Never fails the caller: a refused
/// foreground start (Android 12+ background limits) is logged and playback
/// carries on as before.
pub fn sync(env: &mut JNIEnv, state: i32) {
    if let Err(e) = apply(env, state) {
        let exception = crate::take_exception(env);
        log(&format!("notify: state={state} failed: {e}{exception}"));
    }
}

fn apply(env: &mut JNIEnv, state: i32) -> R<()> {
    let mut guard = fg()?;
    let Some(f) = guard.as_mut() else {
        return Ok(());
    };
    match state {
        STATE_PLAYING => {
            let n = build(env, f)?;
            if !f.started {
                let class = env
                    .get_object_class(f.service.as_obj())
                    .map_err(jerr("service class"))?;
                let intent = env
                    .new_object(
                        "android/content/Intent",
                        "(Landroid/content/Context;Ljava/lang/Class;)V",
                        &[JValue::Object(f.service.as_obj()), JValue::Object(&class)],
                    )
                    .map_err(jerr("service intent"))?;
                env.call_method(
                    f.service.as_obj(),
                    "startForegroundService",
                    "(Landroid/content/Intent;)Landroid/content/ComponentName;",
                    &[JValue::Object(&intent)],
                )
                .map_err(jerr("startForegroundService"))?;
                f.started = true;
            }
            env.call_method(
                f.service.as_obj(),
                "startForeground",
                "(ILandroid/app/Notification;I)V",
                &[
                    JValue::Int(NOTIFICATION_ID),
                    JValue::Object(&n),
                    JValue::Int(FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK),
                ],
            )
            .map_err(jerr("startForeground"))?;
            f.foreground = true;
            log(&format!("notify: foreground=on state={state}"));
        }
        // Preparing the next track: keep whatever we hold.
        STATE_BUFFERING => {}
        STATE_PAUSED => {
            if f.foreground {
                env.call_method(
                    f.service.as_obj(),
                    "stopForeground",
                    "(I)V",
                    &[JValue::Int(STOP_FOREGROUND_DETACH)],
                )
                .map_err(jerr("stopForeground(detach)"))?;
                f.foreground = false;
            }
            if f.started {
                let n = build(env, f)?;
                env.call_method(
                    f.manager.as_obj(),
                    "notify",
                    "(ILandroid/app/Notification;)V",
                    &[JValue::Int(NOTIFICATION_ID), JValue::Object(&n)],
                )
                .map_err(jerr("notify"))?;
                log(&format!(
                    "notify: foreground=off notification=kept state={state}"
                ));
            }
        }
        _ => {
            if !f.started {
                return Ok(());
            }
            if f.foreground {
                env.call_method(
                    f.service.as_obj(),
                    "stopForeground",
                    "(I)V",
                    &[JValue::Int(STOP_FOREGROUND_REMOVE)],
                )
                .map_err(jerr("stopForeground(remove)"))?;
                f.foreground = false;
            }
            env.call_method(
                f.manager.as_obj(),
                "cancel",
                "(I)V",
                &[JValue::Int(NOTIFICATION_ID)],
            )
            .map_err(jerr("cancel"))?;
            env.call_method(f.service.as_obj(), "stopSelf", "()V", &[])
                .map_err(jerr("stopSelf"))?;
            f.started = false;
            log(&format!(
                "notify: foreground=off notification=removed state={state}"
            ));
        }
    }
    Ok(())
}

/// The app's `ic_stat_pharos` drawable, or the system play icon for an APK
/// built without it (the spike's). Looked up once: resource ids are fixed for
/// the life of the process.
fn small_icon(env: &mut JNIEnv, ctx: &JObject) -> R<i32> {
    static ICON: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    if let Some(icon) = ICON.get() {
        return Ok(*icon);
    }
    let res = env
        .call_method(
            ctx,
            "getResources",
            "()Landroid/content/res/Resources;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getResources"))?;
    let pkg = env
        .call_method(ctx, "getPackageName", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getPackageName"))?;
    let name = JObject::from(
        env.new_string("ic_stat_pharos")
            .map_err(jerr("icon name"))?,
    );
    let kind = JObject::from(env.new_string("drawable").map_err(jerr("icon kind"))?);
    let app = env
        .call_method(
            &res,
            "getIdentifier",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
            &[
                JValue::Object(&name),
                JValue::Object(&kind),
                JValue::Object(&pkg),
            ],
        )
        .and_then(|v| v.i())
        .map_err(jerr("getIdentifier"))?;
    let (icon, source) = if app != 0 {
        (app, "app")
    } else {
        let system = env
            .get_static_field("android/R$drawable", "ic_media_play", "I")
            .and_then(|v| v.i())
            .map_err(jerr("ic_media_play"))?;
        (system, "system")
    };
    log(&format!("notify: small_icon={source}"));
    Ok(*ICON.get_or_init(|| icon))
}

fn build<'l>(env: &mut JNIEnv<'l>, f: &Fg) -> R<JObject<'l>> {
    const B: &str = "Landroid/app/Notification$Builder;";
    let id = JObject::from(env.new_string(CHANNEL_ID).map_err(jerr("channel id"))?);
    let b = env
        .new_object(
            "android/app/Notification$Builder",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(f.service.as_obj()), JValue::Object(&id)],
        )
        .map_err(jerr("Notification.Builder"))?;
    let icon = small_icon(env, f.service.as_obj())?;
    env.call_method(&b, "setSmallIcon", format!("(I){B}"), &[JValue::Int(icon)])
        .map_err(jerr("setSmallIcon"))?;
    let title = JObject::from(
        env.new_string(&f.title)
            .map_err(jerr("notification title"))?,
    );
    env.call_method(
        &b,
        "setContentTitle",
        format!("(Ljava/lang/CharSequence;){B}"),
        &[JValue::Object(&title)],
    )
    .map_err(jerr("setContentTitle"))?;
    let text = JObject::from(env.new_string(&f.text).map_err(jerr("notification text"))?);
    env.call_method(
        &b,
        "setContentText",
        format!("(Ljava/lang/CharSequence;){B}"),
        &[JValue::Object(&text)],
    )
    .map_err(jerr("setContentText"))?;
    env.call_method(
        &b,
        "setVisibility",
        format!("(I){B}"),
        &[JValue::Int(VISIBILITY_PUBLIC)],
    )
    .map_err(jerr("setVisibility"))?;
    let style = env
        .new_object("android/app/Notification$MediaStyle", "()V", &[])
        .map_err(jerr("MediaStyle"))?;
    env.call_method(
        &style,
        "setMediaSession",
        "(Landroid/media/session/MediaSession$Token;)Landroid/app/Notification$MediaStyle;",
        &[JValue::Object(f.token.as_obj())],
    )
    .map_err(jerr("setMediaSession"))?;
    env.call_method(
        &b,
        "setStyle",
        format!("(Landroid/app/Notification$Style;){B}"),
        &[JValue::Object(&style)],
    )
    .map_err(jerr("setStyle"))?;
    env.call_method(&b, "build", "()Landroid/app/Notification;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("Notification.build"))
}

/// Leaves the foreground and removes the notification.
pub fn destroy(env: &mut JNIEnv) -> R<()> {
    let taken = fg()?.take();
    if let Some(f) = taken {
        if f.foreground {
            env.call_method(
                f.service.as_obj(),
                "stopForeground",
                "(I)V",
                &[JValue::Int(STOP_FOREGROUND_REMOVE)],
            )
            .map_err(jerr("stopForeground(remove)"))?;
        }
        env.call_method(
            f.manager.as_obj(),
            "cancel",
            "(I)V",
            &[JValue::Int(NOTIFICATION_ID)],
        )
        .map_err(jerr("cancel"))?;
        log("notify: destroyed");
    }
    Ok(())
}
