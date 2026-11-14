//! Audio attributes and focus. Android Auto sends the head unit's media
//! channel to the app holding media audio focus: a player with no
//! attributes that never asks for focus plays on the phone only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use jni::objects::{GlobalRef, JClass, JObject, JValue};
use jni::sys::jint;
use jni::JNIEnv;

use crate::{
    guarded, jerr, log, state_lock, transport, transport_with, R, STATE_PAUSED, STATE_PLAYING,
};

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
/// Whether a transient loss paused us, so the matching gain resumes.
static INTENT: Mutex<Intent> = Mutex::new(Intent::new());

/// The playback intent focus changes act on. `epoch` counts the times the
/// user (or the output going away) cancelled it, so a transient-loss handler
/// that decided to arm it before such a cancel can tell it must not.
#[derive(Debug)]
struct Intent {
    resume_on_gain: bool,
    epoch: u64,
}

impl Intent {
    const fn new() -> Self {
        Self {
            resume_on_gain: false,
            epoch: 0,
        }
    }

    /// Forget a pending resume and invalidate any arm decided earlier.
    fn clear(&mut self) {
        self.resume_on_gain = false;
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Arm the resume, unless a clear happened since `seen`. Returns whether
    /// it armed.
    fn arm(&mut self, seen: u64) -> bool {
        if self.epoch != seen {
            return false;
        }
        self.resume_on_gain = true;
        true
    }

    /// A gain: whether to resume. The intent is spent.
    fn take(&mut self) -> bool {
        std::mem::replace(&mut self.resume_on_gain, false)
    }
}

fn intent() -> std::sync::MutexGuard<'static, Intent> {
    INTENT.lock().unwrap_or_else(PoisonError::into_inner)
}
static DUCKED: AtomicBool = AtomicBool::new(false);

fn audio() -> R<std::sync::MutexGuard<'static, Option<Audio>>> {
    AUDIO.lock().map_err(|_| "audio mutex poisoned".to_owned())
}

/// Builds the media attributes and the focus request for `ctx`, and tells
/// `session` it plays locally with them.
pub fn create(
    env: &mut JNIEnv,
    ctx: &JObject,
    session: &JObject,
    listener_class: &JClass,
) -> R<()> {
    let b = env
        .new_object("android/media/AudioAttributes$Builder", "()V", &[])
        .map_err(jerr("AudioAttributes.Builder"))?;
    let b = env
        .call_method(
            &b,
            "setUsage",
            "(I)Landroid/media/AudioAttributes$Builder;",
            &[JValue::Int(USAGE_MEDIA)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("setUsage"))?;
    let b = env
        .call_method(
            &b,
            "setContentType",
            "(I)Landroid/media/AudioAttributes$Builder;",
            &[JValue::Int(CONTENT_TYPE_MUSIC)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("setContentType"))?;
    let attrs = env
        .call_method(&b, "build", "()Landroid/media/AudioAttributes;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("AudioAttributes.build"))?;
    env.call_method(
        session,
        "setPlaybackToLocal",
        "(Landroid/media/AudioAttributes;)V",
        &[JValue::Object(&attrs)],
    )
    .map_err(jerr("setPlaybackToLocal"))?;

    let name = JObject::from(env.new_string("audio").map_err(jerr("service name"))?);
    let manager = env
        .call_method(
            ctx,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&name)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getSystemService(audio)"))?;
    if manager.is_null() {
        return Err("getSystemService(audio) returned null".into());
    }
    let listener = env
        .new_object(listener_class, "()V", &[])
        .map_err(jerr("focus listener"))?;
    let b = env
        .new_object(
            "android/media/AudioFocusRequest$Builder",
            "(I)V",
            &[JValue::Int(AUDIOFOCUS_GAIN)],
        )
        .map_err(jerr("AudioFocusRequest.Builder"))?;
    let b = env
        .call_method(
            &b,
            "setAudioAttributes",
            "(Landroid/media/AudioAttributes;)Landroid/media/AudioFocusRequest$Builder;",
            &[JValue::Object(&attrs)],
        )
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
        manager: env
            .new_global_ref(&manager)
            .map_err(jerr("global audio manager"))?,
        request: env
            .new_global_ref(&request)
            .map_err(jerr("global focus request"))?,
    });
    log("audio: media attributes set, playback to local");
    Ok(())
}

/// Gives `player` the media attributes. Must run before `setDataSource`.
pub fn apply(env: &mut JNIEnv, player: &JObject) -> R<()> {
    let attrs = audio()?
        .as_ref()
        .map(|a| a.attrs.clone())
        .ok_or("audio not created")?;
    env.call_method(
        player,
        "setAudioAttributes",
        "(Landroid/media/AudioAttributes;)V",
        &[JValue::Object(attrs.as_obj())],
    )
    .map_err(jerr("setAudioAttributes"))?;
    Ok(())
}

/// Asks for media focus before playback starts. False: do not start.
pub fn request(env: &mut JNIEnv) -> R<bool> {
    let Some((manager, req)) = audio()?
        .as_ref()
        .map(|a| (a.manager.clone(), a.request.clone()))
    else {
        return Err("audio not created".into());
    };
    let result = env
        .call_method(
            manager.as_obj(),
            "requestAudioFocus",
            "(Landroid/media/AudioFocusRequest;)I",
            &[JValue::Object(req.as_obj())],
        )
        .and_then(|v| v.i())
        .map_err(jerr("requestAudioFocus"))?;
    intent().clear();
    log(&format!("audio focus request result={result}"));
    Ok(result != AUDIOFOCUS_REQUEST_FAILED)
}

/// Gives focus back: on stop, permanent loss and destroy.
pub fn abandon(env: &mut JNIEnv) -> R<()> {
    let Some((manager, req)) = audio()?
        .as_ref()
        .map(|a| (a.manager.clone(), a.request.clone()))
    else {
        return Ok(());
    };
    let result = env
        .call_method(
            manager.as_obj(),
            "abandonAudioFocusRequest",
            "(Landroid/media/AudioFocusRequest;)I",
            &[JValue::Object(req.as_obj())],
        )
        .and_then(|v| v.i())
        .map_err(jerr("abandonAudioFocusRequest"))?;
    intent().clear();
    log(&format!("audio focus abandoned result={result}"));
    Ok(())
}

pub fn destroy(env: &mut JNIEnv) -> R<()> {
    abandon(env)?;
    *audio()? = None;
    Ok(())
}

/// Forgets a pending resume-on-gain: the user paused, or the output went
/// away, so regaining focus must not start playback again.
pub(crate) fn clear_resume_on_gain() {
    intent().clear();
}

/// The share of the app's level that plays now: lower while ducked.
pub(crate) fn duck_factor() -> f32 {
    if DUCKED.load(Ordering::SeqCst) {
        DUCK_VOLUME
    } else {
        1.0
    }
}

fn set_volume(env: &mut JNIEnv, v: f32) -> R<()> {
    let player = state_lock()?.as_ref().and_then(|s| s.player.clone());
    if let Some(p) = player {
        env.call_method(
            p.as_obj(),
            "setVolume",
            "(FF)V",
            &[JValue::Float(v), JValue::Float(v)],
        )
        .map_err(jerr("setVolume"))?;
    }
    Ok(())
}

pub(crate) fn is_playing(env: &mut JNIEnv) -> R<bool> {
    let player = state_lock()?
        .as_ref()
        .filter(|s| s.prepared)
        .and_then(|s| s.player.clone());
    let Some(p) = player else { return Ok(false) };
    env.call_method(p.as_obj(), "isPlaying", "()Z", &[])
        .and_then(|v| v.z())
        .map_err(jerr("isPlaying"))
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onAudioFocusChange<
    'l,
>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    change: jint,
) {
    guarded(&mut env, "onAudioFocusChange", (), |env| {
        let action = match change {
            AUDIOFOCUS_GAIN => {
                let mut did = Vec::new();
                if DUCKED.swap(false, Ordering::SeqCst) {
                    set_volume(env, crate::command::volume())?;
                    did.push("unduck");
                }
                if intent().take() {
                    transport(env, "start", STATE_PLAYING)?;
                    did.push("resume");
                }
                if did.is_empty() {
                    "none".to_owned()
                } else {
                    did.join("+")
                }
            }
            AUDIOFOCUS_LOSS => {
                if is_playing(env)? {
                    transport(env, "pause", STATE_PAUSED)?;
                }
                abandon(env)?;
                "pause+abandon".to_owned()
            }
            AUDIOFOCUS_LOSS_TRANSIENT => {
                // Read before asking the player: a cancel that lands after
                // this bumps the epoch, and arming below then declines.
                let seen = intent().epoch;
                if is_playing(env)? {
                    // Armed before pausing, and only if nothing cancelled
                    // since `seen`. The lock is not held over the pause.
                    if !intent().arm(seen) {
                        log("focus: a pause cancelled the resume, left paused");
                        return Ok(());
                    }
                    if let Err(e) = transport_with(env, "pause", STATE_PAUSED, false) {
                        clear_resume_on_gain();
                        return Err(e);
                    }
                    "pause".to_owned()
                } else {
                    "none".to_owned()
                }
            }
            AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK => {
                set_volume(env, crate::command::volume() * DUCK_VOLUME)?;
                DUCKED.store(true, Ordering::SeqCst);
                "duck".to_owned()
            }
            _ => "unknown".to_owned(),
        };
        log(&format!("focus change={change} action={action}"));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_forgets_a_pending_resume_on_gain() {
        let mut i = Intent::new();
        assert!(i.arm(i.epoch));
        i.clear();
        assert!(!i.take());
    }

    #[test]
    fn a_cancel_between_the_check_and_the_arm_wins() {
        let mut i = Intent::new();
        // The handler read the epoch, saw the player playing...
        let seen = i.epoch;
        // ...the user paused before it armed.
        i.clear();
        assert!(!i.arm(seen), "the stale arm declines");
        assert!(!i.take(), "so the gain does not resume");
    }

    #[test]
    fn an_arm_with_no_cancel_resumes_once() {
        let mut i = Intent::new();
        assert!(i.arm(i.epoch));
        assert!(i.take());
        assert!(!i.take(), "spent");
    }
}
