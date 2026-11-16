//! Broadcast receivers registered in code, for the service's life only.

use crate::GlobalRef;
use jni::objects::{JObject, JValue};
use jni::Env;

use crate::{jerr, R};

pub struct Registered {
    ctx: GlobalRef,
    receiver: GlobalRef,
}

/// Registers a new `class` on `ctx` for `action` with `Context` receiver
/// `flags`.
pub fn register(
    env: &mut Env,
    ctx: &JObject,
    class: &str,
    action: &str,
    flags: i32,
) -> R<Registered> {
    register_guarded(env, ctx, class, action, None, flags)
}

/// As [`register`], but when `permission` is set only senders holding that
/// permission are delivered.
pub fn register_guarded(
    env: &mut Env,
    ctx: &JObject,
    class: &str,
    action: &str,
    permission: Option<&str>,
    flags: i32,
) -> R<Registered> {
    let receiver = env
        .new_object(
            jni::strings::JNIString::new(class),
            jni::jni_sig!("()V"),
            &[],
        )
        .map_err(jerr("receiver"))?;
    let action_s = JObject::from(env.new_string(action).map_err(jerr("receiver action"))?);
    let filter = env
        .new_object(
            jni::jni_str!("android/content/IntentFilter"),
            jni::jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&action_s)],
        )
        .map_err(jerr("IntentFilter"))?;
    let permission = match permission {
        Some(p) => JObject::from(env.new_string(p).map_err(jerr("receiver permission"))?),
        None => JObject::null(),
    };
    env.call_method(
        ctx,
        jni::jni_str!("registerReceiver"),
        jni::jni_sig!("(Landroid/content/BroadcastReceiver;Landroid/content/IntentFilter;Ljava/lang/String;Landroid/os/Handler;I)Landroid/content/Intent;"),
        &[
            JValue::Object(&receiver),
            JValue::Object(&filter),
            JValue::Object(&permission),
            JValue::Object(&JObject::null()),
            JValue::Int(flags),
        ],
    )
    .map_err(|e| format!("registerReceiver({action}): {e}"))?;
    Ok(Registered {
        ctx: GlobalRef::new(env, ctx).map_err(jerr("global receiver ctx"))?,
        receiver: GlobalRef::new(env, &receiver).map_err(jerr("global receiver"))?,
    })
}

impl Registered {
    pub fn unregister(self, env: &mut Env) -> R<()> {
        env.call_method(
            self.ctx.as_obj(),
            jni::jni_str!("unregisterReceiver"),
            jni::jni_sig!("(Landroid/content/BroadcastReceiver;)V"),
            &[JValue::Object(self.receiver.as_obj())],
        )
        .map_err(jerr("unregisterReceiver"))?;
        Ok(())
    }
}
