//! Starts long tracks where they stopped (`content::Resume` keeps the
//! positions). The player notes its position on pause, stop and track
//! change, and every `RESUME_SAVE_EVERY` while playing, so a killed process
//! loses at most that much.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use jni::objects::{GlobalRef, JValue};
use jni::JNIEnv;

use crate::content::{Resume, ResumeVerdict, RESUME_MIN_TRACK_MS, RESUME_SAVE_EVERY};
use crate::{jerr, log, R, VM};

struct Now {
    id: String,
    duration_ms: u64,
    player: GlobalRef,
}

static STORE: OnceLock<Resume> = OnceLock::new();
static NOW: Mutex<Option<Now>> = Mutex::new(None);
static TICKING: AtomicBool = AtomicBool::new(false);

fn now() -> R<std::sync::MutexGuard<'static, Option<Now>>> {
    NOW.lock().map_err(|_| "resume mutex poisoned".to_owned())
}

/// Loads the saved positions from `files_dir` and starts the save ticker.
pub fn open(files_dir: &str) -> R<()> {
    STORE.get_or_init(|| Resume::open(std::path::Path::new(files_dir)));
    if !TICKING.swap(true, Ordering::SeqCst) {
        std::thread::Builder::new()
            .name("pharos-resume".into())
            .spawn(tick_loop)
            .map_err(|e| format!("spawn resume ticker: {e}"))?;
    }
    Ok(())
}

/// Stops the ticker after its current sleep.
pub fn close() {
    TICKING.store(false, Ordering::SeqCst);
}

fn tick_loop() {
    let Some(vm) = VM.get() else {
        log("resume: ticker has no JavaVM, not saving while playing");
        return;
    };
    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(e) => {
            log(&format!("resume: ticker attach failed: {e}"));
            return;
        }
    };
    while TICKING.load(Ordering::SeqCst) {
        std::thread::sleep(RESUME_SAVE_EVERY);
        if let Err(e) = note_if_playing(&mut env) {
            log(&format!("resume: tick failed: {e}"));
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
        }
    }
}

fn note_if_playing(env: &mut JNIEnv) -> R<()> {
    let guard = now()?;
    let Some(n) = guard.as_ref() else {
        return Ok(());
    };
    let playing = env
        .call_method(n.player.as_obj(), "isPlaying", "()Z", &[])
        .and_then(|v| v.z())
        .map_err(jerr("isPlaying"))?;
    if !playing {
        return Ok(());
    }
    let pos = position(env, &n.player)?;
    record(n, pos, "tick")
}

fn position(env: &mut JNIEnv, player: &GlobalRef) -> R<u64> {
    let pos = env
        .call_method(player.as_obj(), "getCurrentPosition", "()I", &[])
        .and_then(|v| v.i())
        .map_err(jerr("getCurrentPosition"))?;
    Ok(u64::try_from(pos).unwrap_or(0))
}

fn record(n: &Now, pos: u64, why: &str) -> R<()> {
    let Some(store) = STORE.get() else {
        return Ok(());
    };
    match store.record(&n.id, pos, n.duration_ms) {
        // Ticks save every few seconds; only say when something else happened.
        Ok(ResumeVerdict::Saved | ResumeVerdict::Unchanged | ResumeVerdict::Short)
            if why == "tick" => {}
        Ok(ResumeVerdict::Short) => {}
        Ok(v) => log(&format!(
            "resume: id={} verdict={} at_ms={pos} duration_ms={} why={why}",
            n.id,
            v.label(),
            n.duration_ms
        )),
        Err(e) => log(&format!(
            "resume: id={} at_ms={pos} why={why} not saved: {e}",
            n.id
        )),
    }
    Ok(())
}

/// `player` is prepared with `id`. Returns where to seek before starting,
/// and logs the decision.
pub fn prepared(
    env: &mut JNIEnv,
    id: &str,
    duration_ms: u64,
    player: &GlobalRef,
) -> R<Option<u64>> {
    *now()? = Some(Now {
        id: id.to_owned(),
        duration_ms,
        player: player.clone(),
    });
    let Some(store) = STORE.get() else {
        return Ok(None);
    };
    let at = store.position(id, duration_ms);
    match at {
        Some(ms) => {
            env.call_method(
                player.as_obj(),
                "seekTo",
                "(I)V",
                &[JValue::Int(i32::try_from(ms).unwrap_or(i32::MAX))],
            )
            .map_err(jerr("seekTo(resume)"))?;
            log(&format!(
                "resume: id={id} verdict=seek at_ms={ms} duration_ms={duration_ms}"
            ));
        }
        None => {
            let reason = if duration_ms <= RESUME_MIN_TRACK_MS {
                "short"
            } else {
                "none_saved"
            };
            log(&format!(
                "resume: id={id} verdict=start reason={reason} duration_ms={duration_ms}"
            ));
        }
    }
    Ok(at)
}

/// The player paused or stopped at `pos` ms.
pub fn note(pos: i64, why: &str) -> R<()> {
    let guard = now()?;
    let Some(n) = guard.as_ref() else {
        return Ok(());
    };
    record(n, u64::try_from(pos).unwrap_or(0), why)
}

/// The current player is about to be released: save where it was.
pub fn leave(env: &mut JNIEnv, why: &str) -> R<()> {
    let taken = now()?.take();
    if let Some(n) = taken {
        // Never stops the caller from releasing the player.
        match position(env, &n.player) {
            Ok(pos) => record(&n, pos, why)?,
            Err(e) => log(&format!(
                "resume: id={} why={why} position unreadable, not saved: {e}",
                n.id
            )),
        }
    }
    Ok(())
}

/// The current track played to its end, or failed: forget the player
/// without asking it anything. `finished` drops the saved position.
pub fn drop_current(finished: bool) -> R<()> {
    let taken = now()?.take();
    if let (Some(n), true) = (taken, finished) {
        record(&n, n.duration_ms, "completed")?;
    }
    Ok(())
}
