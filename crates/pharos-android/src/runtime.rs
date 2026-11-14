//! The entry for the phone app: [`runtime`] sets up the shared state either
//! entry point needs, and its [`Player`] sends the UI's commands to the same
//! code the car's callbacks use.
//!
//! Commands queue to one thread attached to the JVM, so they apply in the
//! order sent and the UI never waits on JNI. Between commands that thread
//! reads the playhead about once a second while playing, for the snapshot.

use std::sync::mpsc;
use std::time::Duration;

use super::*;

/// How often the playhead reaches the snapshot while playing. The UI treats
/// a jump larger than a few of these as a seek.
const POSITION_EVERY: Duration = Duration::from_secs(1);

/// The phone UI's handle on the native player. Every method returns at once;
/// the outcome arrives on [`Player::state`].
pub struct Player {
    tx: mpsc::Sender<Command>,
}

impl Player {
    fn send(&self, command: Command) {
        let name = format!("{command:?}");
        if self.tx.send(command).is_err() {
            log(&format!(
                "command source=app verdict=dropped reason=command_thread_gone command={name}"
            ));
            snapshot::note_error("the player's command thread is gone");
        }
    }

    /// Replace the queue with `ids` (item ids) and play `start_index`.
    pub fn play(&self, ids: Vec<String>, start_index: usize) {
        self.send(Command::Play {
            ids,
            index: start_index,
        });
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    pub fn resume(&self) {
        self.send(Command::Resume);
    }

    pub fn seek(&self, position_ms: u64) {
        self.send(Command::Seek(position_ms));
    }

    pub fn next(&self) {
        self.send(Command::Next);
    }

    pub fn previous(&self) {
        self.send(Command::Previous);
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    /// Replace the queue with `ids`, where `current_index` is the track now
    /// playing. It keeps playing; when `ids[current_index]` is not that track
    /// nothing changes and the verdict is logged.
    pub fn set_queue(&self, ids: Vec<String>, current_index: usize) {
        self.send(Command::SetQueue {
            ids,
            index: current_index,
        });
    }

    /// The output level, 0.0 (silent) to 1.0 (full), for this track and the
    /// ones after it. Out-of-range values are clamped.
    pub fn set_volume(&self, level: f32) {
        set_volume(level);
        self.send(Command::Volume);
    }

    /// A receiver that always holds the latest snapshot.
    pub fn state(&self) -> tokio::sync::watch::Receiver<snapshot::Snapshot> {
        snapshot::subscribe()
    }
}

static PLAYER: OnceLock<Player> = OnceLock::new();
/// Held while the runtime starts, so two entry points do not both start it.
static STARTING: Mutex<()> = Mutex::new(());

/// Sets up what the service and the app share: the JavaVM, the app's
/// classes, the library, resume positions, and the command thread. Safe to
/// call more than once and from either entry point; later calls return the
/// same player.
///
/// Call it on the main thread (service `onCreate`, or the app's start): only
/// there does class lookup see the app's own classes. `ctx` is any Context.
pub fn runtime(env: &mut JNIEnv, ctx: &JObject) -> R<&'static Player> {
    let _starting = STARTING
        .lock()
        .map_err(|_| "runtime start mutex poisoned".to_owned())?;
    if let Some(player) = PLAYER.get() {
        return Ok(player);
    }
    if VM.get().is_none() {
        let _ = VM.set(env.get_java_vm().map_err(jerr("get_java_vm"))?);
    }
    cache_classes(env)?;
    // The test folder still works when these fail.
    if let Err(e) = connect_library(env, ctx) {
        log(&format!("runtime: library not connected: {e}"));
    }
    if let Err(e) = files_dir(env, ctx).and_then(|dir| resume::open(&dir)) {
        log(&format!("runtime: resume positions unavailable: {e}"));
    }
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("pharos-player".into())
        .spawn(move || command_loop(rx))
        .map_err(|e| format!("spawn command thread: {e}"))?;
    log("runtime: started");
    Ok(PLAYER.get_or_init(|| Player { tx }))
}

fn cache_classes(env: &mut JNIEnv) -> R<()> {
    if CLASSES.get().is_some() {
        return Ok(());
    }
    let listener = env
        .find_class("com/alisonjenkins/pharos/auto/PharosPlayerListener")
        .map_err(jerr("find PharosPlayerListener"))?;
    let compat_item = env
        .find_class(compat::ITEM_CLASS)
        .map_err(jerr("find compat MediaItem"))?;
    let _ = CLASSES.set(Classes {
        listener: env
            .new_global_ref(&listener)
            .map_err(jerr("global listener class"))?,
        compat_item: env
            .new_global_ref(&compat_item)
            .map_err(jerr("global compat class"))?,
    });
    Ok(())
}

fn command_loop(rx: mpsc::Receiver<Command>) {
    let Some(vm) = VM.get() else {
        log("command thread: no JavaVM, app commands are dropped");
        return;
    };
    let mut env = match vm.attach_current_thread_permanently() {
        Ok(env) => env,
        Err(e) => {
            log(&format!("command thread: attach failed: {e}"));
            return;
        }
    };
    loop {
        match rx.recv_timeout(POSITION_EVERY) {
            Ok(command) => {
                guarded(&mut env, "app command", (), |env| {
                    apply(env, command, Source::App)
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                guarded(&mut env, "position tick", (), tick_position);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                log("command thread: player dropped, stopping");
                return;
            }
        }
    }
}

/// Reads the playhead into the snapshot while playing.
fn tick_position(env: &mut JNIEnv) -> R<()> {
    if snapshot::current().state != snapshot::PlayState::Playing {
        return Ok(());
    }
    let player = {
        let guard = state_lock()?;
        match guard.as_ref() {
            Some(st) if st.prepared => st.player.clone(),
            _ => None,
        }
    };
    let Some(player) = player else {
        return Ok(());
    };
    let pos = env
        .call_method(player.as_obj(), "getCurrentPosition", "()I", &[])
        .and_then(|v| v.i())
        .map_err(jerr("getCurrentPosition"))?;
    snapshot::note_position(u64::try_from(pos).unwrap_or(0));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_player_whose_thread_is_gone_reports_it_on_the_snapshot() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let player = Player { tx };
        let state = player.state();
        player.pause();
        assert_eq!(
            state.borrow().error.as_deref(),
            Some("the player's command thread is gone")
        );
    }

    #[test]
    fn commands_reach_the_thread_in_the_order_sent() {
        let (tx, rx) = mpsc::channel();
        let player = Player { tx };
        player.play(vec!["1".into(), "2".into()], 1);
        player.pause();
        player.seek(5_000);
        player.resume();
        let got: Vec<Command> = rx.try_iter().collect();
        assert_eq!(
            got,
            [
                Command::Play {
                    ids: vec!["1".into(), "2".into()],
                    index: 1
                },
                Command::Pause,
                Command::Seek(5_000),
                Command::Resume,
            ]
        );
    }
}
