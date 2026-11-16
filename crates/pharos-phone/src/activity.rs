//! Calls that need the app's Activity, reached through the context tao
//! records at start-up.

use std::sync::Once;

use jni::objects::JObject;
use jni::Env;

/// Runs `f` with the activity on this thread, clearing any exception it
/// leaves so the next JNI call works.
fn with_activity(f: impl FnOnce(&mut Env, &JObject) -> Result<(), String>) -> Result<(), String> {
    let ctx = ndk_context::android_context();
    // SAFETY: tao put the process's JavaVM and the activity in the context
    // at start-up, and both live as long as the app.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) };
    vm.attach_current_thread_for_scope(|env| -> Result<Result<(), String>, jni::errors::Error> {
        // The context is an app-lifetime handle, not a local reference of
        // this frame, so borrow it instead of taking it over.
        let raw: jni::sys::jobject = ctx.context().cast();
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw)? };
        let result = f(env, &activity);
        if result.is_err() && env.exception_check() {
            env.exception_clear();
        }
        Ok(result)
    })
    .map_err(|e| e.to_string())?
}

/// Starts the car's media service, while the activity is in the foreground;
/// its `onCreate` sets up the player the ports use.
pub fn start_media_service() {
    if let Err(e) = with_activity(pharos_android::start_service) {
        log::warn!("pharos: media service not started error={e}");
    }
}

/// Asks for the notification permission the first time music plays, so the
/// playback notification shows; later plays do not ask again.
pub fn ask_notifications_on_first_play() {
    static ASKED: Once = Once::new();
    ASKED.call_once(|| {
        let asked = with_activity(|env, activity| {
            pharos_android::ask_notification_permission(env, activity).map(|_| ())
        });
        if let Err(e) = asked {
            log::warn!("pharos: notification permission not asked error={e}");
        }
    });
}
