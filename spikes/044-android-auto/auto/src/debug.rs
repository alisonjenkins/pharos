//! A debug broadcast for testing on a real phone without its screen:
//!
//! `adb shell am broadcast -a com.alisonjenkins.pharos.auto.DEBUG_OFFLINE --ez on true`
//!
//! makes the server unreachable (see `content::Library::debug_offline`);
//! `--ez on false` restores it. Registered in code, for the service's life
//! only. Exported, because the shell is another app; the worst a caller can
//! do is make this app act offline.

use std::sync::Mutex;

use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;

use crate::{guarded, jerr, log, R, LIBRARY};

pub const ACTION: &str = "com.alisonjenkins.pharos.auto.DEBUG_OFFLINE";
/// `Context.RECEIVER_EXPORTED` (API 33): the shell's broadcasts reach it.
const RECEIVER_EXPORTED: i32 = 2;

struct Registered {
    ctx: GlobalRef,
    receiver: GlobalRef,
}

static RECEIVER: Mutex<Option<Registered>> = Mutex::new(None);

pub fn register(env: &mut JNIEnv, ctx: &JObject) -> R<()> {
    let mut guard = RECEIVER.lock().map_err(|_| "debug receiver mutex poisoned".to_owned())?;
    if guard.is_some() {
        return Ok(());
    }
    let receiver = env
        .new_object("com/alisonjenkins/pharos/auto/PharosDebugReceiver", "()V", &[])
        .map_err(jerr("PharosDebugReceiver"))?;
    let action = JObject::from(env.new_string(ACTION).map_err(jerr("debug action"))?);
    let filter = env
        .new_object("android/content/IntentFilter", "(Ljava/lang/String;)V", &[JValue::Object(&action)])
        .map_err(jerr("IntentFilter"))?;
    env.call_method(
        ctx,
        "registerReceiver",
        "(Landroid/content/BroadcastReceiver;Landroid/content/IntentFilter;I)Landroid/content/Intent;",
        &[JValue::Object(&receiver), JValue::Object(&filter), JValue::Int(RECEIVER_EXPORTED)],
    )
    .map_err(jerr("registerReceiver(debug)"))?;
    *guard = Some(Registered {
        ctx: env.new_global_ref(ctx).map_err(jerr("global debug ctx"))?,
        receiver: env.new_global_ref(&receiver).map_err(jerr("global debug receiver"))?,
    });
    log(&format!("debug: receiver registered action={ACTION}"));
    Ok(())
}

pub fn unregister(env: &mut JNIEnv) -> R<()> {
    let taken = RECEIVER.lock().map_err(|_| "debug receiver mutex poisoned".to_owned())?.take();
    if let Some(r) = taken {
        env.call_method(
            r.ctx.as_obj(),
            "unregisterReceiver",
            "(Landroid/content/BroadcastReceiver;)V",
            &[JValue::Object(r.receiver.as_obj())],
        )
        .map_err(jerr("unregisterReceiver(debug)"))?;
        log("debug: receiver unregistered");
    }
    Ok(())
}

fn has_extra(env: &mut JNIEnv, intent: &JObject, name: &str) -> R<bool> {
    let key = JObject::from(env.new_string(name).map_err(jerr("extra name"))?);
    env.call_method(intent, "hasExtra", "(Ljava/lang/String;)Z", &[JValue::Object(&key)])
        .and_then(|v| v.z())
        .map_err(jerr("hasExtra"))
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosDebugReceiver_onReceive<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    _ctx: JObject<'l>,
    intent: JObject<'l>,
) {
    guarded(&mut env, "debug onReceive", (), |env| {
        let Some(lib) = LIBRARY.get() else {
            log("debug: no library, ignored");
            return Ok(());
        };
        if has_extra(env, &intent, "on")? {
            let key = JObject::from(env.new_string("on").map_err(jerr("extra name"))?);
            let on = env
                .call_method(&intent, "getBooleanExtra", "(Ljava/lang/String;Z)Z", &[JValue::Object(&key), JValue::Bool(0)])
                .and_then(|v| v.z())
                .map_err(jerr("getBooleanExtra(on)"))?;
            lib.debug_offline(on);
        } else {
            log("debug: broadcast with no known extra, ignored");
        }
        Ok(())
    })
}
