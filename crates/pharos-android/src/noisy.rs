//! Pauses when audio is about to leave headphones or a Bluetooth sink for
//! the phone speaker (`ACTION_AUDIO_BECOMING_NOISY`).

use std::sync::Mutex;

use jni::objects::JObject;
use jni::JNIEnv;

use crate::receiver::{self, Registered};
use crate::{focus, guarded, log, transport, R, STATE_PAUSED};

pub const ACTION: &str = "android.media.AUDIO_BECOMING_NOISY";
/// `Context.RECEIVER_NOT_EXPORTED` (API 33): system broadcasts still arrive.
const RECEIVER_NOT_EXPORTED: i32 = 4;

static RECEIVER: Mutex<Option<Registered>> = Mutex::new(None);

pub fn register(env: &mut JNIEnv, ctx: &JObject) -> R<()> {
    let mut guard = RECEIVER
        .lock()
        .map_err(|_| "noisy receiver mutex poisoned".to_owned())?;
    if guard.is_some() {
        return Ok(());
    }
    *guard = Some(receiver::register(
        env,
        ctx,
        "com/alisonjenkins/pharos/auto/PharosNoisyReceiver",
        ACTION,
        RECEIVER_NOT_EXPORTED,
    )?);
    log(&format!("noisy: receiver registered action={ACTION}"));
    Ok(())
}

pub fn unregister(env: &mut JNIEnv) -> R<()> {
    let taken = RECEIVER
        .lock()
        .map_err(|_| "noisy receiver mutex poisoned".to_owned())?
        .take();
    if let Some(r) = taken {
        r.unregister(env)?;
        log("noisy: receiver unregistered");
    }
    Ok(())
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosNoisyReceiver_onReceive<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    _ctx: JObject<'l>,
    _intent: JObject<'l>,
) {
    guarded(&mut env, "noisy onReceive", (), |env| {
        let action = if focus::is_playing(env)? {
            transport(env, "pause", STATE_PAUSED)?;
            "pause"
        } else {
            "none"
        };
        log(&format!("noisy: action={action}"));
        Ok(())
    })
}
