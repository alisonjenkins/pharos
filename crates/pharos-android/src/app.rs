//! What the phone app calls from its own threads: start the media service,
//! reach its player and library once it runs, tell the car about a sign-in,
//! and ask for the notification permission.
//!
//! The app's Rust runs on dx's thread, where class lookup cannot see the
//! app's own classes, so it does not call [`runtime`] itself. It starts the
//! media service, whose `onCreate` runs [`runtime`] on the main thread; until
//! then [`player`] is `None`.

use super::*;

/// The media service, by name, so the intent needs no class lookup.
const SERVICE_CLASS: &str = "com.alisonjenkins.pharos.auto.PharosMediaService";

/// `PackageManager.PERMISSION_GRANTED`.
const PERMISSION_GRANTED: i32 = 0;
/// Android 13, the first to need a runtime ask for notifications.
const TIRAMISU: i32 = 33;
const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";
/// The request code `onRequestPermissionsResult` would see; nothing reads it.
const NOTIFICATION_REQUEST: i32 = 44;

/// The player, once the media service has started the runtime.
pub fn player() -> Option<&'static Player> {
    runtime::started()
}

/// The library, once the media service has connected it.
pub fn library() -> Option<&'static content::Library> {
    LIBRARY.get()
}

/// Starts the media service, so its `onCreate` sets up the runtime the app's
/// commands need. Call it while the activity is in the foreground: a
/// background start throws. Safe to repeat; a running service ignores it.
pub fn start_service(env: &mut JNIEnv, ctx: &JObject) -> R<()> {
    let intent = env
        .new_object("android/content/Intent", "()V", &[])
        .map_err(jerr("Intent"))?;
    let name = JObject::from(
        env.new_string(SERVICE_CLASS)
            .map_err(jerr("service name"))?,
    );
    env.call_method(
        &intent,
        "setClassName",
        "(Landroid/content/Context;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(ctx), JValue::Object(&name)],
    )
    .map_err(jerr("Intent.setClassName"))?;
    let started = env
        .call_method(
            ctx,
            "startService",
            "(Landroid/content/Intent;)Landroid/content/ComponentName;",
            &[JValue::Object(&intent)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("startService"))?;
    let verdict = if started.is_null() {
        "not_found"
    } else {
        "started"
    };
    log(&format!("app start_service verdict={verdict}"));
    if started.is_null() {
        return Err(format!("startService: no service {SERVICE_CLASS}"));
    }
    Ok(())
}

/// The phone signed in or out: the car re-reads the token file, so its
/// lists follow without a sign-in step of its own.
pub fn credentials_changed() {
    let Some(lib) = LIBRARY.get() else {
        log("app credentials_changed verdict=skipped reason=library_not_connected");
        return;
    };
    match lib.reload_credentials() {
        Ok(()) => {
            log("app credentials_changed verdict=reloaded");
            notify_changed();
        }
        Err(e) => log(&format!(
            "app credentials_changed verdict=failed reason={}",
            e.redacted()
        )),
    }
}

/// What the notification ask did, for the log and the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationAsk {
    /// Before Android 13 no ask is needed.
    NotNeeded,
    AlreadyGranted,
    Asked,
}

impl NotificationAsk {
    pub fn label(self) -> &'static str {
        match self {
            NotificationAsk::NotNeeded => "not_needed",
            NotificationAsk::AlreadyGranted => "already_granted",
            NotificationAsk::Asked => "asked",
        }
    }
}

/// Asks for `POST_NOTIFICATIONS` when the system needs it and it is not yet
/// granted, so the playback notification shows. `activity` must be an
/// Activity; the system shows its own dialog.
pub fn ask_notification_permission(env: &mut JNIEnv, activity: &JObject) -> R<NotificationAsk> {
    let sdk = env
        .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
        .and_then(|v| v.i())
        .map_err(jerr("SDK_INT"))?;
    let ask = if sdk < TIRAMISU {
        NotificationAsk::NotNeeded
    } else {
        let name = JObject::from(
            env.new_string(POST_NOTIFICATIONS)
                .map_err(jerr("permission name"))?,
        );
        let granted = env
            .call_method(
                activity,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
                &[JValue::Object(&name)],
            )
            .and_then(|v| v.i())
            .map_err(jerr("checkSelfPermission"))?;
        if granted == PERMISSION_GRANTED {
            NotificationAsk::AlreadyGranted
        } else {
            let names = env
                .new_object_array(1, "java/lang/String", &name)
                .map_err(jerr("permission array"))?;
            env.call_method(
                activity,
                "requestPermissions",
                "([Ljava/lang/String;I)V",
                &[JValue::Object(&names), JValue::Int(NOTIFICATION_REQUEST)],
            )
            .map_err(jerr("requestPermissions"))?;
            NotificationAsk::Asked
        }
    };
    log(&format!(
        "app notification_permission verdict={} sdk={sdk}",
        ask.label()
    ));
    Ok(ask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_ask_labels_are_distinct() {
        let mut labels = [
            NotificationAsk::NotNeeded,
            NotificationAsk::AlreadyGranted,
            NotificationAsk::Asked,
        ]
        .map(NotificationAsk::label);
        labels.sort_unstable();
        let n = labels.len();
        let mut unique = labels.to_vec();
        unique.dedup();
        assert_eq!(unique.len(), n);
    }

    #[test]
    fn nothing_is_reachable_before_the_service_starts() {
        assert!(player().is_none());
        assert!(library().is_none());
        credentials_changed();
    }
}
