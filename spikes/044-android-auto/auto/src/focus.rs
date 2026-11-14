//! Audio attributes and focus. Android Auto sends the head unit's media
//! channel to the app holding media audio focus: a player with no
//! attributes that never asks for focus plays on the phone only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use jni::objects::{GlobalRef, JClass, JObject, JValue};
use jni::sys::jint;
use jni::JNIEnv;

use crate::{guarded, jerr, log, state_lock, transport, R, STATE_PAUSED, STATE_PLAYING};

const USAGE_MEDIA: i32 = 1;
const CONTENT_TYPE_MUSIC: i32 = 2;
const AUDIOFOCUS_GAIN: i32 = 1;
const AUDIOFOCUS_LOSS: i32 = -1;
const AUDIOFOCUS_LOSS_TRANSIENT: i32 = -2;
const AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK: i32 = -3;
const AUDIOFOCUS_REQUEST_FAILED: i32 = 0;
/// Volume while another app speaks over us (navigation prompts).
const DUCK_VOLUME: f32 = 0.2;

struct Audio {
    attrs: GlobalRef,
    manager: GlobalRef,
    request: GlobalRef,
}

static AUDIO: Mutex<Option<Audio>> = Mutex::new(None);
/// Set when a transient loss paused us, so the matching gain resumes.
static RESUME_ON_GAIN: AtomicBool = AtomicBool::new(false);
static DUCKED: AtomicBool = AtomicBool::new(false);

fn audio() -> R<std::sync::MutexGuard<'static, Option<Audio>>> {
    AUDIO.lock().map_err(|_| "audio mutex poisoned".to_owned())
}

/// Builds the media attributes and the focus request for `ctx`, and tells
/// `session` it plays locally with them.
pub fn create(env: &mut JNIEnv, ctx: &JObject, session: &JObject, listener_class: &JClass) -> R<()> {
    let b = env.new_object("android/media/AudioAttributes$Builder", "()V", &[]).map_err(jerr("AudioAttributes.Builder"))?;
    let b = env
        .call_method(&b, "setUsage", "(I)Landroid/media/AudioAttributes$Builder;", &[JValue::Int(USAGE_MEDIA)])
        .and_then(|v| v.l())
        .map_err(jerr("setUsage"))?;
    let b = env
        .call_method(&b, "setContentType", "(I)Landroid/media/AudioAttributes$Builder;", &[JValue::Int(CONTENT_TYPE_MUSIC)])
        .and_then(|v| v.l())
        .map_err(jerr("setContentType"))?;
    let attrs = env
        .call_method(&b, "build", "()Landroid/media/AudioAttributes;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("AudioAttributes.build"))?;
    env.call_method(session, "setPlaybackToLocal", "(Landroid/media/AudioAttributes;)V", &[JValue::Object(&attrs)])
        .map_err(jerr("setPlaybackToLocal"))?;

    let name = JObject::from(env.new_string("audio").map_err(jerr("service name"))?);
    let manager = env
        .call_method(ctx, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])
        .and_then(|v| v.l())
        .map_err(jerr("getSystemService(audio)"))?;
    if manager.is_null() {
        return Err("getSystemService(audio) returned null".into());
    }
    let listener = env.new_object(listener_class, "()V", &[]).map_err(jerr("focus listener"))?;
    let b = env
        .new_object("android/media/AudioFocusRequest$Builder", "(I)V", &[JValue::Int(AUDIOFOCUS_GAIN)])
        .map_err(jerr("AudioFocusRequest.Builder"))?;
    let b = env
        .call_method(&b, "setAudioAttributes", "(Landroid/media/AudioAttributes;)Landroid/media/AudioFocusRequest$Builder;", &[JValue::Object(&attrs)])
        .and_then(|v| v.l())
        .map_err(jerr("focus setAudioAttributes"))?;
    let b = env
        .call_method(
            &b,
            "setOnAudioFocusChangeListener",
            "(Landroid/media/AudioManager$OnAudioFocusChangeListener;)Landroid/media/AudioFocusRequest$Builder;",
            &[JValue::Object(&listener)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("setOnAudioFocusChangeListener"))?;
    let request = env
        .call_method(&b, "build", "()Landroid/media/AudioFocusRequest;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("AudioFocusRequest.build"))?;
    *audio()? = Some(Audio {
        attrs: env.new_global_ref(&attrs).map_err(jerr("global attrs"))?,
        manager: env.new_global_ref(&manager).map_err(jerr("global audio manager"))?,
        request: env.new_global_ref(&request).map_err(jerr("global focus request"))?,
    });
    log("audio: media attributes set, playback to local");
    Ok(())
}

/// Gives `player` the media attributes. Must run before `setDataSource`.
pub fn apply(env: &mut JNIEnv, player: &JObject) -> R<()> {
    let attrs = audio()?.as_ref().map(|a| a.attrs.clone()).ok_or("audio not created")?;
    env.call_method(player, "setAudioAttributes", "(Landroid/media/AudioAttributes;)V", &[JValue::Object(attrs.as_obj())])
        .map_err(jerr("setAudioAttributes"))?;
    Ok(())
}

/// Asks for media focus before playback starts. False: do not start.
pub fn request(env: &mut JNIEnv) -> R<bool> {
    let Some((manager, req)) = audio()?.as_ref().map(|a| (a.manager.clone(), a.request.clone())) else {
        return Err("audio not created".into());
    };
    let result = env
        .call_method(manager.as_obj(), "requestAudioFocus", "(Landroid/media/AudioFocusRequest;)I", &[JValue::Object(req.as_obj())])
        .and_then(|v| v.i())
        .map_err(jerr("requestAudioFocus"))?;
    RESUME_ON_GAIN.store(false, Ordering::SeqCst);
    log(&format!("audio focus request result={result}"));
    Ok(result != AUDIOFOCUS_REQUEST_FAILED)
}

/// Gives focus back: on stop, permanent loss and destroy.
pub fn abandon(env: &mut JNIEnv) -> R<()> {
    let Some((manager, req)) = audio()?.as_ref().map(|a| (a.manager.clone(), a.request.clone())) else {
        return Ok(());
    };
    let result = env
        .call_method(manager.as_obj(), "abandonAudioFocusRequest", "(Landroid/media/AudioFocusRequest;)I", &[JValue::Object(req.as_obj())])
        .and_then(|v| v.i())
        .map_err(jerr("abandonAudioFocusRequest"))?;
    RESUME_ON_GAIN.store(false, Ordering::SeqCst);
    log(&format!("audio focus abandoned result={result}"));
    Ok(())
}

pub fn destroy(env: &mut JNIEnv) -> R<()> {
    abandon(env)?;
    *audio()? = None;
    Ok(())
}

fn set_volume(env: &mut JNIEnv, v: f32) -> R<()> {
    let player = state_lock()?.as_ref().and_then(|s| s.player.clone());
    if let Some(p) = player {
        env.call_method(p.as_obj(), "setVolume", "(FF)V", &[JValue::Float(v), JValue::Float(v)]).map_err(jerr("setVolume"))?;
    }
    Ok(())
}

fn is_playing(env: &mut JNIEnv) -> R<bool> {
    let player = state_lock()?.as_ref().filter(|s| s.prepared).and_then(|s| s.player.clone());
    let Some(p) = player else { return Ok(false) };
    env.call_method(p.as_obj(), "isPlaying", "()Z", &[]).and_then(|v| v.z()).map_err(jerr("isPlaying"))
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onAudioFocusChange<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    change: jint,
) {
    guarded(&mut env, "onAudioFocusChange", (), |env| {
        let action = match change {
            AUDIOFOCUS_GAIN => {
                let mut did = Vec::new();
                if DUCKED.swap(false, Ordering::SeqCst) {
                    set_volume(env, 1.0)?;
                    did.push("unduck");
                }
                if RESUME_ON_GAIN.swap(false, Ordering::SeqCst) {
                    transport(env, "start", STATE_PLAYING)?;
                    did.push("resume");
                }
                if did.is_empty() { "none".to_owned() } else { did.join("+") }
            }
            AUDIOFOCUS_LOSS => {
                if is_playing(env)? {
                    transport(env, "pause", STATE_PAUSED)?;
                }
                abandon(env)?;
                "pause+abandon".to_owned()
            }
            AUDIOFOCUS_LOSS_TRANSIENT => {
                if is_playing(env)? {
                    transport(env, "pause", STATE_PAUSED)?;
                    RESUME_ON_GAIN.store(true, Ordering::SeqCst);
                    "pause".to_owned()
                } else {
                    "none".to_owned()
                }
            }
            AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK => {
                set_volume(env, DUCK_VOLUME)?;
                DUCKED.store(true, Ordering::SeqCst);
                "duck".to_owned()
            }
            _ => "unknown".to_owned(),
        };
        log(&format!("focus change={change} action={action}"));
        Ok(())
    })
}
