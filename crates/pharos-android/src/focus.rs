//! Audio attributes and focus. Android Auto sends the head unit's media
//! channel to the app holding media audio focus: a player with no
//! attributes that never asks for focus plays on the phone only.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::GlobalRef;
use jni::objects::{JClass, JObject, JValue};
use jni::sys::jint;
use jni::{Env, EnvUnowned};

use crate::{
    guarded_native, jerr, log, state_lock, transport, transport_with, R, STATE_PAUSED,
    STATE_PLAYING,
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

/// The playback intent focus changes act on. Invariant: a gain never
/// resumes after a user pause until a user play. `user_paused` is sticky and
/// is checked at the gain, so it does not matter in what order a pause, the
/// player's own state and a transient-loss handler interleave.
#[derive(Debug)]
struct Intent {
    resume_on_gain: bool,
    /// The user (or the output going away) paused; cleared by a user play.
    user_paused: bool,
}

impl Intent {
    const fn new() -> Self {
        Self {
            resume_on_gain: false,
            user_paused: false,
        }
    }

    /// Forget a pending resume (focus requested or abandoned).
    fn clear(&mut self) {
        self.resume_on_gain = false;
    }

    /// The user paused or stopped, or the output went away.
    fn user_pause(&mut self) {
        self.resume_on_gain = false;
        self.user_paused = true;
    }

    /// The user played.
    fn user_play(&mut self) {
        self.user_paused = false;
    }

    /// A transient loss paused the player. Always records; a user pause in
    /// flight still wins at the gain.
    fn arm(&mut self) {
        self.resume_on_gain = true;
    }

    /// Whether an auto-resume that took the intent may still start: a user
    /// pause that landed since `take` says no.
    fn may_resume(&self) -> bool {
        !self.user_paused
    }

    /// A gain: whether to resume. The intent is spent.
    fn take(&mut self) -> bool {
        let resume = std::mem::replace(&mut self.resume_on_gain, false);
        resume && !self.user_paused
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
    env: &mut Env,
    ctx: &JObject,
    session: &JObject,
    listener_class: &jni::refs::Global<JClass<'static>>,
) -> R<()> {
    let b = env
        .new_object(
            jni::jni_str!("android/media/AudioAttributes$Builder"),
            jni::jni_sig!("()V"),
            &[],
        )
        .map_err(jerr("AudioAttributes.Builder"))?;
    let b = env
        .call_method(
            &b,
            jni::jni_str!("setUsage"),
            jni::jni_sig!("(I)Landroid/media/AudioAttributes$Builder;"),
            &[JValue::Int(USAGE_MEDIA)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("setUsage"))?;
    let b = env
        .call_method(
            &b,
            jni::jni_str!("setContentType"),
            jni::jni_sig!("(I)Landroid/media/AudioAttributes$Builder;"),
            &[JValue::Int(CONTENT_TYPE_MUSIC)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("setContentType"))?;
    let attrs = env
        .call_method(
            &b,
            jni::jni_str!("build"),
            jni::jni_sig!("()Landroid/media/AudioAttributes;"),
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("AudioAttributes.build"))?;
    env.call_method(
        session,
        jni::jni_str!("setPlaybackToLocal"),
        jni::jni_sig!("(Landroid/media/AudioAttributes;)V"),
        &[JValue::Object(&attrs)],
    )
    .map_err(jerr("setPlaybackToLocal"))?;

    let name = JObject::from(env.new_string("audio").map_err(jerr("service name"))?);
    let manager = env
        .call_method(
            ctx,
            jni::jni_str!("getSystemService"),
            jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::Object(&name)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getSystemService(audio)"))?;
    if manager.is_null() {
        return Err("getSystemService(audio) returned null".into());
    }
    let listener = env
        .new_object(listener_class, jni::jni_sig!("()V"), &[])
        .map_err(jerr("focus listener"))?;
    let b = env
        .new_object(
            jni::jni_str!("android/media/AudioFocusRequest$Builder"),
            jni::jni_sig!("(I)V"),
            &[JValue::Int(AUDIOFOCUS_GAIN)],
        )
        .map_err(jerr("AudioFocusRequest.Builder"))?;
    let b = env
        .call_method(
            &b,
            jni::jni_str!("setAudioAttributes"),
            jni::jni_sig!(
                "(Landroid/media/AudioAttributes;)Landroid/media/AudioFocusRequest$Builder;"
            ),
            &[JValue::Object(&attrs)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("focus setAudioAttributes"))?;
    let b = env
        .call_method(
            &b,
            jni::jni_str!("setOnAudioFocusChangeListener"),
            jni::jni_sig!("(Landroid/media/AudioManager$OnAudioFocusChangeListener;)Landroid/media/AudioFocusRequest$Builder;"),
            &[JValue::Object(&listener)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("setOnAudioFocusChangeListener"))?;
    let request = env
        .call_method(
            &b,
            jni::jni_str!("build"),
            jni::jni_sig!("()Landroid/media/AudioFocusRequest;"),
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("AudioFocusRequest.build"))?;
    *audio()? = Some(Audio {
        attrs: GlobalRef::new(env, &attrs).map_err(jerr("global attrs"))?,
        manager: GlobalRef::new(env, &manager).map_err(jerr("global audio manager"))?,
        request: GlobalRef::new(env, &request).map_err(jerr("global focus request"))?,
    });
    log("audio: media attributes set, playback to local");
    Ok(())
}

/// Gives `player` the media attributes. Must run before `setDataSource`.
pub fn apply(env: &mut Env, player: &JObject) -> R<()> {
    let attrs = audio()?
        .as_ref()
        .map(|a| a.attrs.clone())
        .ok_or("audio not created")?;
    env.call_method(
        player,
        jni::jni_str!("setAudioAttributes"),
        jni::jni_sig!("(Landroid/media/AudioAttributes;)V"),
        &[JValue::Object(attrs.as_obj())],
    )
    .map_err(jerr("setAudioAttributes"))?;
    Ok(())
}

/// Asks for media focus before playback starts. False: do not start.
pub fn request(env: &mut Env) -> R<bool> {
    let Some((manager, req)) = audio()?
        .as_ref()
        .map(|a| (a.manager.clone(), a.request.clone()))
    else {
        return Err("audio not created".into());
    };
    let result = env
        .call_method(
            manager.as_obj(),
            jni::jni_str!("requestAudioFocus"),
            jni::jni_sig!("(Landroid/media/AudioFocusRequest;)I"),
            &[JValue::Object(req.as_obj())],
        )
        .and_then(|v| v.i())
        .map_err(jerr("requestAudioFocus"))?;
    intent().clear();
    log(&format!("audio focus request result={result}"));
    Ok(result != AUDIOFOCUS_REQUEST_FAILED)
}

/// Gives focus back: on stop, permanent loss and destroy.
pub fn abandon(env: &mut Env) -> R<()> {
    let Some((manager, req)) = audio()?
        .as_ref()
        .map(|a| (a.manager.clone(), a.request.clone()))
    else {
        return Ok(());
    };
    let result = env
        .call_method(
            manager.as_obj(),
            jni::jni_str!("abandonAudioFocusRequest"),
            jni::jni_sig!("(Landroid/media/AudioFocusRequest;)I"),
            &[JValue::Object(req.as_obj())],
        )
        .and_then(|v| v.i())
        .map_err(jerr("abandonAudioFocusRequest"))?;
    intent().clear();
    log(&format!("audio focus abandoned result={result}"));
    Ok(())
}

pub fn destroy(env: &mut Env) -> R<()> {
    abandon(env)?;
    *audio()? = None;
    Ok(())
}

/// Forgets a pending resume-on-gain: the user paused, or the output went
/// away, so regaining focus must not start playback again.
pub(crate) fn note_user_pause() {
    intent().user_pause();
}

/// The user started playback: a later gain may resume it again.
pub(crate) fn note_user_play() {
    intent().user_play();
}

/// The share of the app's level that plays now: lower while ducked.
pub(crate) fn duck_factor() -> f32 {
    if DUCKED.load(Ordering::SeqCst) {
        DUCK_VOLUME
    } else {
        1.0
    }
}

fn set_volume(env: &mut Env, v: f32) -> R<()> {
    let player = state_lock()?.as_ref().and_then(|s| s.player.clone());
    if let Some(p) = player {
        env.call_method(
            p.as_obj(),
            jni::jni_str!("setVolume"),
            jni::jni_sig!("(FF)V"),
            &[JValue::Float(v), JValue::Float(v)],
        )
        .map_err(jerr("setVolume"))?;
    }
    Ok(())
}

pub(crate) fn is_playing(env: &mut Env) -> R<bool> {
    let player = state_lock()?
        .as_ref()
        .filter(|s| s.prepared)
        .and_then(|s| s.player.clone());
    let Some(p) = player else { return Ok(false) };
    env.call_method(
        p.as_obj(),
        jni::jni_str!("isPlaying"),
        jni::jni_sig!("()Z"),
        &[],
    )
    .and_then(|v| v.z())
    .map_err(jerr("isPlaying"))
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onAudioFocusChange<
    'l,
>(
    mut env: EnvUnowned<'l>,
    _this: JObject<'l>,
    change: jint,
) {
    guarded_native(&mut env, "onAudioFocusChange", (), |env| {
        let action = match change {
            AUDIOFOCUS_GAIN => {
                let mut did = Vec::new();
                if DUCKED.swap(false, Ordering::SeqCst) {
                    set_volume(env, crate::command::volume())?;
                    did.push("unduck");
                }
                if intent().take() {
                    // The system's start, not the user's: it must not clear
                    // `user_paused`. Re-checked just before; a pause during
                    // the start call pauses afterwards, so the last wins.
                    if intent().may_resume() {
                        transport_with(env, "start", STATE_PLAYING, false)?;
                        did.push("resume");
                    } else {
                        did.push("resume-skipped");
                    }
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
                if is_playing(env)? {
                    // Armed before pausing. A user pause in flight sets
                    // `user_paused`, which the gain checks, so the order of
                    // the two does not matter. No lock is held over the pause.
                    intent().arm();
                    if let Err(e) = transport_with(env, "pause", STATE_PAUSED, false) {
                        intent().clear();
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
        i.arm();
        i.clear();
        assert!(!i.take());
    }

    #[test]
    fn an_arm_with_no_user_pause_resumes_once() {
        let mut i = Intent::new();
        i.arm();
        assert!(i.take());
        assert!(!i.take(), "spent");
    }

    #[test]
    fn a_user_pause_before_the_arm_wins() {
        let mut i = Intent::new();
        i.user_pause();
        i.arm();
        assert!(!i.take(), "gain never resumes after a user pause");
    }

    #[test]
    fn a_user_pause_after_the_arm_wins() {
        let mut i = Intent::new();
        i.arm();
        i.user_pause();
        assert!(!i.take());
    }

    #[test]
    fn a_pause_after_take_still_blocks_the_resume() {
        let mut i = Intent::new();
        i.arm();
        assert!(i.take());
        i.user_pause();
        assert!(!i.may_resume());
    }

    #[test]
    fn an_auto_resume_does_not_clear_a_user_pause() {
        let mut i = Intent::new();
        i.user_pause();
        i.arm();
        assert!(!i.take());
        assert!(!i.may_resume(), "only user_play clears it");
    }

    #[test]
    fn a_user_play_lets_a_later_gain_resume() {
        let mut i = Intent::new();
        i.user_pause();
        assert!(!i.take());
        i.user_play();
        i.arm();
        assert!(i.take());
    }
}
