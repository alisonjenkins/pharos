//! The media notification and foreground state. Playing holds the service
//! in the foreground, so playback survives a locked phone with no car
//! bound to it; pausing leaves the foreground but keeps the notification;
//! stopping removes it and lets the service stop.
//!
//! A `MediaStyle` notification tied to the session is exempt from the
//! `POST_NOTIFICATIONS` runtime permission (API 33), and on API 33+ the
//! system draws its controls from the session's playback state.

use std::sync::Mutex;

use crate::GlobalRef;
use jni::objects::{JObject, JValue};
use jni::Env;

use crate::{jerr, log, R, STATE_BUFFERING, STATE_PAUSED, STATE_PLAYING};

const CHANNEL_ID: &str = "playback";
const CHANNEL_NAME: &str = "Playback";
/// `NotificationManager.IMPORTANCE_LOW`: shown, but silent.
const IMPORTANCE_LOW: i32 = 2;
const NOTIFICATION_ID: i32 = 1;
/// `ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK`, matching the manifest.
const FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK: i32 = 2;
/// `startForeground(int, Notification, int)` and service types arrived in API 29.
const FOREGROUND_TYPE_SDK: i32 = 29;
/// `Service.STOP_FOREGROUND_REMOVE` / `STOP_FOREGROUND_DETACH`.
const STOP_FOREGROUND_REMOVE: i32 = 1;
const STOP_FOREGROUND_DETACH: i32 = 2;
/// `Notification.VISIBILITY_PUBLIC`: the lock screen shows the controls.
const VISIBILITY_PUBLIC: i32 = 1;

struct Fg {
    service: GlobalRef,
    token: GlobalRef,
    manager: GlobalRef,
    sdk: i32,
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
pub fn create(env: &mut Env, service: &JObject, token: &JObject) -> R<()> {
    let name = JObject::from(
        env.new_string("notification")
            .map_err(jerr("service name"))?,
    );
    let manager = env
        .call_method(
            service,
            jni::jni_str!("getSystemService"),
            jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::Object(&name)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getSystemService(notification)"))?;
    if manager.is_null() {
        return Err("getSystemService(notification) returned null".into());
    }
    let sdk = env
        .get_static_field(
            jni::jni_str!("android/os/Build$VERSION"),
            jni::jni_str!("SDK_INT"),
            jni::jni_sig!("I"),
        )
        .and_then(|v| v.i())
        .map_err(jerr("SDK_INT"))?;
    let id = JObject::from(env.new_string(CHANNEL_ID).map_err(jerr("channel id"))?);
    let label = JObject::from(env.new_string(CHANNEL_NAME).map_err(jerr("channel name"))?);
    let channel = env
        .new_object(
            jni::jni_str!("android/app/NotificationChannel"),
            jni::jni_sig!("(Ljava/lang/String;Ljava/lang/CharSequence;I)V"),
            &[
                JValue::Object(&id),
                JValue::Object(&label),
                JValue::Int(IMPORTANCE_LOW),
            ],
        )
        .map_err(jerr("NotificationChannel"))?;
    env.call_method(
        &manager,
        jni::jni_str!("createNotificationChannel"),
        jni::jni_sig!("(Landroid/app/NotificationChannel;)V"),
        &[JValue::Object(&channel)],
    )
    .map_err(jerr("createNotificationChannel"))?;
    *fg()? = Some(Fg {
        service: GlobalRef::new(env, service).map_err(jerr("global fg service"))?,
        token: GlobalRef::new(env, token).map_err(jerr("global fg token"))?,
        manager: GlobalRef::new(env, &manager).map_err(jerr("global notification manager"))?,
        sdk,
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
pub fn sync(env: &mut Env, state: i32) {
    if let Err(e) = apply(env, state) {
        let exception = crate::take_exception(env);
        log(&format!("notify: state={state} failed: {e}{exception}"));
    }
}

fn apply(env: &mut Env, state: i32) -> R<()> {
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
                        jni::jni_str!("android/content/Intent"),
                        jni::jni_sig!("(Landroid/content/Context;Ljava/lang/Class;)V"),
                        &[JValue::Object(f.service.as_obj()), JValue::Object(&class)],
                    )
                    .map_err(jerr("service intent"))?;
                env.call_method(
                    f.service.as_obj(),
                    jni::jni_str!("startForegroundService"),
                    jni::jni_sig!("(Landroid/content/Intent;)Landroid/content/ComponentName;"),
                    &[JValue::Object(&intent)],
                )
                .map_err(jerr("startForegroundService"))?;
                f.started = true;
            }
            if has_foreground_type(f.sdk) {
                env.call_method(
                    f.service.as_obj(),
                    jni::jni_str!("startForeground"),
                    jni::jni_sig!("(ILandroid/app/Notification;I)V"),
                    &[
                        JValue::Int(NOTIFICATION_ID),
                        JValue::Object(&n),
                        JValue::Int(FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK),
                    ],
                )
            } else {
                env.call_method(
                    f.service.as_obj(),
                    jni::jni_str!("startForeground"),
                    jni::jni_sig!("(ILandroid/app/Notification;)V"),
                    &[JValue::Int(NOTIFICATION_ID), JValue::Object(&n)],
                )
            }
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
                    jni::jni_str!("stopForeground"),
                    jni::jni_sig!("(I)V"),
                    &[JValue::Int(STOP_FOREGROUND_DETACH)],
                )
                .map_err(jerr("stopForeground(detach)"))?;
                f.foreground = false;
            }
            if f.started {
                let n = build(env, f)?;
                env.call_method(
                    f.manager.as_obj(),
                    jni::jni_str!("notify"),
                    jni::jni_sig!("(ILandroid/app/Notification;)V"),
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
                    jni::jni_str!("stopForeground"),
                    jni::jni_sig!("(I)V"),
                    &[JValue::Int(STOP_FOREGROUND_REMOVE)],
                )
                .map_err(jerr("stopForeground(remove)"))?;
                f.foreground = false;
            }
            env.call_method(
                f.manager.as_obj(),
                jni::jni_str!("cancel"),
                jni::jni_sig!("(I)V"),
                &[JValue::Int(NOTIFICATION_ID)],
            )
            .map_err(jerr("cancel"))?;
            env.call_method(
                f.service.as_obj(),
                jni::jni_str!("stopSelf"),
                jni::jni_sig!("()V"),
                &[],
            )
            .map_err(jerr("stopSelf"))?;
            f.started = false;
            log(&format!(
                "notify: foreground=off notification=removed state={state}"
            ));
        }
    }
    Ok(())
}

fn has_foreground_type(sdk: i32) -> bool {
    sdk >= FOREGROUND_TYPE_SDK
}

/// The app's `ic_stat_pharos` drawable, or the system play icon for an APK
/// built without it (the spike's). Looked up once: resource ids are fixed for
/// the life of the process.
fn small_icon(env: &mut Env, ctx: &JObject) -> R<i32> {
    static ICON: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    if let Some(icon) = ICON.get() {
        return Ok(*icon);
    }
    let res = env
        .call_method(
            ctx,
            jni::jni_str!("getResources"),
            jni::jni_sig!("()Landroid/content/res/Resources;"),
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getResources"))?;
    let pkg = env
        .call_method(
            ctx,
            jni::jni_str!("getPackageName"),
            jni::jni_sig!("()Ljava/lang/String;"),
            &[],
        )
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
            jni::jni_str!("getIdentifier"),
            jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I"),
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
            .get_static_field(
                jni::jni_str!("android/R$drawable"),
                jni::jni_str!("ic_media_play"),
                jni::jni_sig!("I"),
            )
            .and_then(|v| v.i())
            .map_err(jerr("ic_media_play"))?;
        (system, "system")
    };
    log(&format!("notify: small_icon={source}"));
    Ok(*ICON.get_or_init(|| icon))
}

fn build<'l>(env: &mut Env<'l>, f: &Fg) -> R<JObject<'l>> {
    let id = JObject::from(env.new_string(CHANNEL_ID).map_err(jerr("channel id"))?);
    let b = env
        .new_object(
            jni::jni_str!("android/app/Notification$Builder"),
            jni::jni_sig!("(Landroid/content/Context;Ljava/lang/String;)V"),
            &[JValue::Object(f.service.as_obj()), JValue::Object(&id)],
        )
        .map_err(jerr("Notification.Builder"))?;
    let icon = small_icon(env, f.service.as_obj())?;
    env.call_method(
        &b,
        jni::jni_str!("setSmallIcon"),
        jni::jni_sig!("(I)Landroid/app/Notification$Builder;"),
        &[JValue::Int(icon)],
    )
    .map_err(jerr("setSmallIcon"))?;
    let title = JObject::from(
        env.new_string(&f.title)
            .map_err(jerr("notification title"))?,
    );
    env.call_method(
        &b,
        jni::jni_str!("setContentTitle"),
        jni::jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;"),
        &[JValue::Object(&title)],
    )
    .map_err(jerr("setContentTitle"))?;
    let text = JObject::from(env.new_string(&f.text).map_err(jerr("notification text"))?);
    env.call_method(
        &b,
        jni::jni_str!("setContentText"),
        jni::jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;"),
        &[JValue::Object(&text)],
    )
    .map_err(jerr("setContentText"))?;
    env.call_method(
        &b,
        jni::jni_str!("setVisibility"),
        jni::jni_sig!("(I)Landroid/app/Notification$Builder;"),
        &[JValue::Int(VISIBILITY_PUBLIC)],
    )
    .map_err(jerr("setVisibility"))?;
    let style = env
        .new_object(
            jni::jni_str!("android/app/Notification$MediaStyle"),
            jni::jni_sig!("()V"),
            &[],
        )
        .map_err(jerr("MediaStyle"))?;
    env.call_method(
        &style,
        jni::jni_str!("setMediaSession"),
        jni::jni_sig!(
            "(Landroid/media/session/MediaSession$Token;)Landroid/app/Notification$MediaStyle;"
        ),
        &[JValue::Object(f.token.as_obj())],
    )
    .map_err(jerr("setMediaSession"))?;
    env.call_method(
        &b,
        jni::jni_str!("setStyle"),
        jni::jni_sig!("(Landroid/app/Notification$Style;)Landroid/app/Notification$Builder;"),
        &[JValue::Object(&style)],
    )
    .map_err(jerr("setStyle"))?;
    env.call_method(
        &b,
        jni::jni_str!("build"),
        jni::jni_sig!("()Landroid/app/Notification;"),
        &[],
    )
    .and_then(|v| v.l())
    .map_err(jerr("Notification.build"))
}

/// Leaves the foreground and removes the notification.
pub fn destroy(env: &mut Env) -> R<()> {
    let taken = fg()?.take();
    if let Some(f) = taken {
        if f.foreground {
            env.call_method(
                f.service.as_obj(),
                jni::jni_str!("stopForeground"),
                jni::jni_sig!("(I)V"),
                &[JValue::Int(STOP_FOREGROUND_REMOVE)],
            )
            .map_err(jerr("stopForeground(remove)"))?;
        }
        env.call_method(
            f.manager.as_obj(),
            jni::jni_str!("cancel"),
            jni::jni_sig!("(I)V"),
            &[JValue::Int(NOTIFICATION_ID)],
        )
        .map_err(jerr("cancel"))?;
        log("notify: destroyed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_typed_start_foreground_needs_api_29() {
        assert!(!has_foreground_type(26));
        assert!(!has_foreground_type(28));
        assert!(has_foreground_type(29));
        assert!(has_foreground_type(34));
    }
}
