//! Starts long tracks where they stopped (`content::Resume` keeps the
//! positions). The player notes its position on pause, stop and track
//! change, and every `RESUME_SAVE_EVERY` while playing, so a killed process
//! loses at most that much.
//!
//! A book's part also tells the server where it is, every
//! `REPORT_EVERY_TICKS` ticks while playing and on pause, stop and track
//! change, so the phone's own player and other devices resume there too.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use jni::objects::{GlobalRef, JValue};
use jni::JNIEnv;

use crate::{jerr, log, LIBRARY, R, VM};
use pharos_content::{Progress, Resume, ResumeVerdict, RESUME_MIN_TRACK_MS, RESUME_SAVE_EVERY};

/// Ticks between server reports while a part plays: with the 5 s tick,
/// about every 10 s, so another device resumes within that of the car.
const REPORT_EVERY_TICKS: u32 = 2;

struct Now {
    id: String,
    duration_ms: u64,
    player: GlobalRef,
    /// A book's part: its progress goes to the server as well.
    report: bool,
    ticks: u32,
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
    let mut guard = now()?;
    let Some(n) = guard.as_mut() else {
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
    n.ticks += 1;
    if n.ticks % REPORT_EVERY_TICKS == 0 {
        tell_server(n, pos, Progress::Playing);
    }
    record(n, pos, "tick")
}

/// Sends `p` at `pos` for a book's part; nothing for anything else. The
/// library logs the outcome; a failure here only means it was not queued.
fn tell_server(n: &Now, pos: u64, p: Progress) {
    if !n.report {
        return;
    }
    let Some(lib) = LIBRARY.get() else {
        return;
    };
    if let Err(e) = lib.progress(&n.id, pos, p) {
        log(&format!(
            "car progress {} {} at_ms={pos}: outcome=not_sent: {}",
            p.label(),
            n.id,
            e.redacted()
        ));
    }
}

/// What a pause or stop of the player tells the server.
fn progress_for(why: &str) -> Progress {
    if why == "pause" {
        Progress::Paused
    } else {
        Progress::Stopped
    }
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

/// Where to start: the server's position for a book's part (`server_ms`,
/// 0 for none) while it is inside the track, else the one kept on the
/// phone, the offline fallback.
fn start_at(server_ms: u64, local: Option<u64>, duration_ms: u64) -> (Option<u64>, &'static str) {
    if server_ms > 0 && server_ms < duration_ms {
        (Some(server_ms), "server")
    } else {
        (local, "phone")
    }
}

/// `player` is prepared with `id`. Returns where to seek before starting,
/// and logs the decision. `report` sends its progress to the server too.
pub fn prepared(
    env: &mut JNIEnv,
    id: &str,
    duration_ms: u64,
    server_ms: u64,
    report: bool,
    player: &GlobalRef,
) -> R<Option<u64>> {
    let local = STORE.get().and_then(|s| s.position(id, duration_ms));
    let (at, from) = start_at(server_ms, local, duration_ms);
    let n = Now {
        id: id.to_owned(),
        duration_ms,
        player: player.clone(),
        report,
        ticks: 0,
    };
    tell_server(&n, at.unwrap_or(0), Progress::Playing);
    *now()? = Some(n);
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
                "resume: id={id} verdict=seek from={from} at_ms={ms} duration_ms={duration_ms}"
            ));
        }
        None => {
            let reason = if server_ms > 0 {
                "server_past_end"
            } else if duration_ms <= RESUME_MIN_TRACK_MS {
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
    let pos = u64::try_from(pos).unwrap_or(0);
    tell_server(n, pos, progress_for(why));
    record(n, pos, why)
}

/// The current player is about to be released: save where it was.
pub fn leave(env: &mut JNIEnv, why: &str) -> R<()> {
    let taken = now()?.take();
    if let Some(n) = taken {
        // Never stops the caller from releasing the player.
        match position(env, &n.player) {
            Ok(pos) => {
                tell_server(&n, pos, Progress::Stopped);
                record(&n, pos, why)?;
            }
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
        tell_server(&n, n.duration_ms, Progress::Stopped);
        record(&n, n.duration_ms, "completed")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_servers_position_wins_and_the_phones_is_the_fallback() {
        assert_eq!(
            start_at(61_000, Some(5_000), 600_000),
            (Some(61_000), "server")
        );
        assert_eq!(start_at(0, Some(5_000), 600_000), (Some(5_000), "phone"));
        assert_eq!(start_at(0, None, 600_000), (None, "phone"));
        assert_eq!(
            start_at(700_000, None, 600_000),
            (None, "phone"),
            "past the end: not a place to seek"
        );
    }

    #[test]
    fn a_pause_tells_the_server_paused_and_anything_else_stopped() {
        assert_eq!(progress_for("pause"), Progress::Paused);
        assert_eq!(progress_for("stop"), Progress::Stopped);
    }
}
