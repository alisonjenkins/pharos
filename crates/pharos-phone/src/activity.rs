//! Calls that need the app's Activity, reached through the context tao
//! records at start-up.

use std::sync::Once;

use jni::objects::JObject;
use jni::JNIEnv;

/// Runs `f` with the activity on this thread, clearing any exception it
/// leaves so the next JNI call works.
fn with_activity(
    f: impl FnOnce(&mut JNIEnv, &JObject) -> Result<(), String>,
) -> Result<(), String> {
    let ctx = ndk_context::android_context();
    // SAFETY: tao put the process's JavaVM and the activity in the context
    // at start-up, and both live as long as the app.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
    let result = f(&mut env, &activity);
    if result.is_err() && env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    result
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
