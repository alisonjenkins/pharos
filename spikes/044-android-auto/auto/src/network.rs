//! Tells the library when the phone gains or loses its default network, so
//! downloads pause and browsing answers from saved listings at once instead
//! of waiting for a timeout in a black spot.

use std::sync::Mutex;

use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;

use crate::{guarded, jerr, log, R, LIBRARY};

struct Watch {
    manager: GlobalRef,
    callback: GlobalRef,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

/// Registers the default-network callback. The platform calls `onAvailable`
/// at once when a network is up.
pub fn watch(env: &mut JNIEnv, ctx: &JObject) -> R<()> {
    let mut guard = WATCH.lock().map_err(|_| "network mutex poisoned".to_owned())?;
    if guard.is_some() {
        return Ok(());
    }
    let name = JObject::from(env.new_string("connectivity").map_err(jerr("service name"))?);
    let manager = env
        .call_method(ctx, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])
        .and_then(|v| v.l())
        .map_err(jerr("getSystemService(connectivity)"))?;
    if manager.is_null() {
        return Err("getSystemService(connectivity) returned null".into());
    }
    let callback = env
        .new_object("com/alisonjenkins/pharos/auto/PharosNetworkCallback", "()V", &[])
        .map_err(jerr("PharosNetworkCallback"))?;
    env.call_method(
        &manager,
        "registerDefaultNetworkCallback",
        "(Landroid/net/ConnectivityManager$NetworkCallback;)V",
        &[JValue::Object(&callback)],
    )
    .map_err(jerr("registerDefaultNetworkCallback"))?;
    *guard = Some(Watch {
        manager: env.new_global_ref(&manager).map_err(jerr("global connectivity"))?,
        callback: env.new_global_ref(&callback).map_err(jerr("global network callback"))?,
    });
    log("network: default-network callback registered");
    Ok(())
}

/// Unregisters the callback, so a recreated service does not stack them.
pub fn unwatch(env: &mut JNIEnv) -> R<()> {
    let taken = WATCH.lock().map_err(|_| "network mutex poisoned".to_owned())?.take();
    if let Some(w) = taken {
        env.call_method(
            w.manager.as_obj(),
            "unregisterNetworkCallback",
            "(Landroid/net/ConnectivityManager$NetworkCallback;)V",
            &[JValue::Object(w.callback.as_obj())],
        )
        .map_err(jerr("unregisterNetworkCallback"))?;
        log("network: callback unregistered");
    }
    Ok(())
}

fn set_online(online: bool) {
    match LIBRARY.get() {
        Some(lib) => {
            lib.set_online(online);
            log(&format!("network online={online}"));
        }
        None => log(&format!("network online={online}: no library, ignored")),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosNetworkCallback_onAvailable<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    _network: JObject<'l>,
) {
    guarded(&mut env, "onAvailable", (), |_| {
        set_online(true);
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosNetworkCallback_onLost<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    _network: JObject<'l>,
) {
    guarded(&mut env, "onLost", (), |_| {
        set_online(false);
        Ok(())
    })
}
