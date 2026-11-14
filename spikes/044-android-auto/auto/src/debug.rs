//! A debug broadcast for testing on a real phone without its screen:
//!
//! `adb shell am broadcast -a com.alisonjenkins.pharos.auto.DEBUG_OFFLINE --ez on true`
//!
//! makes the server unreachable (see `content::Library::debug_offline`);
//! `--ez on false` restores it. `--el cap_bytes N` sets the cache cap (see
//! `content::Library::set_cache_cap`). Registered in code, for the
//! service's life only. Exported, because the shell is another app; the
//! worst a caller can do is make this app act offline or shrink its cache.

use std::sync::Mutex;

use jni::objects::{JObject, JValue};
use jni::JNIEnv;

use crate::receiver::{self, Registered};
use crate::{guarded, jerr, log, R, LIBRARY};

pub const ACTION: &str = "com.alisonjenkins.pharos.auto.DEBUG_OFFLINE";
/// `Context.RECEIVER_EXPORTED` (API 33): the shell's broadcasts reach it.
const RECEIVER_EXPORTED: i32 = 2;

static RECEIVER: Mutex<Option<Registered>> = Mutex::new(None);

pub fn register(env: &mut JNIEnv, ctx: &JObject) -> R<()> {
    let mut guard = RECEIVER.lock().map_err(|_| "debug receiver mutex poisoned".to_owned())?;
    if guard.is_some() {
        return Ok(());
    }
    *guard = Some(receiver::register(env, ctx, "com/alisonjenkins/pharos/auto/PharosDebugReceiver", ACTION, RECEIVER_EXPORTED)?);
    log(&format!("debug: receiver registered action={ACTION}"));
    Ok(())
}

pub fn unregister(env: &mut JNIEnv) -> R<()> {
    let taken = RECEIVER.lock().map_err(|_| "debug receiver mutex poisoned".to_owned())?.take();
    if let Some(r) = taken {
        r.unregister(env)?;
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
        let mut known = false;
        if has_extra(env, &intent, "on")? {
            let key = JObject::from(env.new_string("on").map_err(jerr("extra name"))?);
            let on = env
                .call_method(&intent, "getBooleanExtra", "(Ljava/lang/String;Z)Z", &[JValue::Object(&key), JValue::Bool(0)])
                .and_then(|v| v.z())
                .map_err(jerr("getBooleanExtra(on)"))?;
            lib.debug_offline(on);
            known = true;
        }
        if has_extra(env, &intent, "cap_bytes")? {
            let key = JObject::from(env.new_string("cap_bytes").map_err(jerr("extra name"))?);
            let cap = env
                .call_method(&intent, "getLongExtra", "(Ljava/lang/String;J)J", &[JValue::Object(&key), JValue::Long(-1)])
                .and_then(|v| v.j())
                .map_err(jerr("getLongExtra(cap_bytes)"))?;
            match u64::try_from(cap) {
                Ok(bytes) => match lib.set_cache_cap(bytes) {
                    Ok(()) => log(&format!("debug: cache cap set bytes={bytes}")),
                    Err(e) => log(&format!("debug: cache cap bytes={bytes} not set: {e}")),
                },
                // `--ei` or a missing `--el` reads as the -1 default.
                Err(_) => log(&format!("debug: cap_bytes={cap} ignored: send a non-negative long with --el")),
            }
            known = true;
        }
        if !known {
            log("debug: broadcast with no known extra, ignored");
        }
        Ok(())
    })
}
