//! The Rust side of the Pharos Android app: everything behind the generated
//! `PharosMediaService`, `PharosSessionCallback`, `PharosPlayerListener` and
//! `PharosSearchActivity` classes (see `pharos-classgen`). No Java or Kotlin
//! source exists; each Java entry point lands in one of these functions.
//!
//! The browse tree is the user's Pharos library (`content::Library`), plus a
//! Test folder of tones when built for the host check (see `fixture`). Library
//! calls block on the
//! network, so they run on worker threads attached to the JVM, and browse
//! results are detached and sent from there. Every call logs its inputs so a
//! logcat line proves each hop reached Rust; logs carry ids, never titles.

mod art;
mod browse;
mod command;

mod compat;

mod debug;
mod fixture;

mod focus;

mod foreground;

mod network;

mod noisy;
mod player;

mod receiver;

mod resume;
mod runtime;
mod search;
mod service;
mod session;
pub mod snapshot;

mod voice;

use pharos_content as content;

use browse::*;
use command::*;
use player::*;
pub use runtime::{runtime, Player};
use search::*;
use service::*;
use session::*;

use std::path::Path;

use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::{Mutex, OnceLock};

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};

use jni::sys::{jlong, jobject};

use jni::{JNIEnv, JavaVM};

const STATE_STOPPED: i32 = 1;

const STATE_PAUSED: i32 = 2;

const STATE_PLAYING: i32 = 3;

const STATE_ERROR: i32 = 7;

const STATE_BUFFERING: i32 = 6;

// PlaybackState.ACTION_*: STOP | PAUSE | PLAY | SKIP_TO_PREVIOUS |
// SKIP_TO_NEXT | SEEK_TO | PLAY_PAUSE | PLAY_FROM_MEDIA_ID | PLAY_FROM_SEARCH.
const ACTIONS: i64 = 1 | 2 | 4 | 16 | 32 | 256 | 512 | 1024 | 2048 | 16384 | 32768 | 65536;

const FLAG_BROWSABLE: i32 = 1;

const FLAG_PLAYABLE: i32 = 2;

const ROOT: &str = "root";

/// Browse lists kept so a played track can skip through its siblings.
const REMEMBERED_LISTS: usize = 32;

/// One row the car shows: a test entry, a library node, or a notice.
#[derive(Debug, Clone)]
struct Item {
    id: String,
    title: String,
    subtitle: String,
    browsable: bool,
    playable: bool,
    /// An http(s) image; empty for none.
    art: String,
    /// 0 when unknown.
    duration_ms: i64,
    /// Empty when unknown or not a track.
    album: String,
}

impl Item {
    fn of_node(n: content::Node) -> Self {
        Self {
            id: n.id,
            title: n.title,
            subtitle: n.subtitle,
            browsable: n.browsable,
            playable: n.playable,
            art: n.art_url.unwrap_or_default(),
            duration_ms: i64::try_from(n.duration_ms).unwrap_or(0),
            album: n.album,
        }
    }

    /// A row that is neither browsable nor playable: it only tells the
    /// driver something (a failure, the library being offline).
    fn notice(title: &str, why: &str) -> Self {
        Self {
            id: "notice".into(),
            title: title.to_owned(),
            subtitle: why.to_owned(),
            browsable: false,
            playable: false,
            art: String::new(),
            duration_ms: 0,
            album: String::new(),
        }
    }
}

struct State {
    service: GlobalRef,
    session: GlobalRef,
    player: Option<GlobalRef>,
    /// False while `player` is still preparing; transport calls wait for it.
    prepared: bool,
    current: Option<Item>,
    /// What skip next/previous moves through; holds `current`.
    queue: Vec<Item>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

static VM: OnceLock<JavaVM> = OnceLock::new();

static LIBRARY: OnceLock<content::Library> = OnceLock::new();

/// App classes, looked up on the main thread: a thread attached from Rust
/// finds classes through the system loader, which does not see the app's.
struct Classes {
    listener: GlobalRef,
    compat_item: GlobalRef,
}

static CLASSES: OnceLock<Classes> = OnceLock::new();

/// Bumped by every play request; a library play that finishes resolving
/// after a newer request is dropped instead of replacing it.
static PLAY_GEN: AtomicU64 = AtomicU64::new(0);

/// Recent browse and search lists' playable rows, newest last.
static LISTS: Mutex<Vec<Vec<Item>>> = Mutex::new(Vec::new());

fn log(msg: &str) {
    #[cfg(target_os = "android")]
    {
        #[link(name = "log")]
        extern "C" {
            fn __android_log_write(
                prio: i32,
                tag: *const std::ffi::c_char,
                text: *const std::ffi::c_char,
            ) -> i32;
        }
        let Ok(text) = std::ffi::CString::new(msg.replace('\0', " ")) else {
            return;
        };
        // 4 = ANDROID_LOG_INFO.
        // SAFETY: both strings are NUL-terminated and live across the call.
        unsafe {
            __android_log_write(4, c"pharos-auto".as_ptr(), text.as_ptr());
        }
    }
    #[cfg(not(target_os = "android"))]
    eprintln!("pharos-auto: {msg}");
}

type R<T> = Result<T, String>;

fn jerr(what: &str) -> impl Fn(jni::errors::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Runs `f`; on error logs it with `context`, describes and clears any Java
/// exception so the call returns to Java cleanly.
fn guarded<T>(
    env: &mut JNIEnv,
    context: &str,
    fallback: T,
    f: impl FnOnce(&mut JNIEnv) -> R<T>,
) -> T {
    match f(env) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("{context} failed: {e}"));
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            fallback
        }
    }
}

fn string(env: &mut JNIEnv, s: &JString) -> R<String> {
    if s.is_null() {
        return Ok(String::new());
    }
    env.get_string(s)
        .map(Into::into)
        .map_err(jerr("get_string"))
}

fn state_lock() -> R<std::sync::MutexGuard<'static, Option<State>>> {
    STATE.lock().map_err(|_| "state mutex poisoned".to_owned())
}

impl ::log::Log for ContentLog {
    fn enabled(&self, m: &::log::Metadata) -> bool {
        m.level() <= ::log::Level::Info
    }
    fn log(&self, r: &::log::Record) {
        if self.enabled(r.metadata()) {
            log(&format!("content {}: {}", r.level(), r.args()));
        }
    }
    fn flush(&self) {}
}

/// Every `Java_` export, as addresses. The app's cdylib calls [`link`] so
/// the linker keeps these: an rlib's `#[no_mangle]` functions that nothing
/// in the final library reaches are dropped, and the JVM only finds out on
/// the first call. `jvm-check/run.sh` lists the .so's symbols to prove it.
pub fn exports() -> [*const (); 33] {
    [
        art::Java_com_alisonjenkins_pharos_auto_PharosArtProvider_delete as *const (),
        art::Java_com_alisonjenkins_pharos_auto_PharosArtProvider_getType as *const (),
        art::Java_com_alisonjenkins_pharos_auto_PharosArtProvider_insert as *const (),
        art::Java_com_alisonjenkins_pharos_auto_PharosArtProvider_onCreate as *const (),
        art::Java_com_alisonjenkins_pharos_auto_PharosArtProvider_openFile as *const (),
        art::Java_com_alisonjenkins_pharos_auto_PharosArtProvider_query as *const (),
        art::Java_com_alisonjenkins_pharos_auto_PharosArtProvider_update as *const (),
        compat::Java_com_alisonjenkins_pharos_auto_PharosCompatHandler_handleMessage as *const (),
        debug::Java_com_alisonjenkins_pharos_auto_PharosDebugReceiver_onReceive as *const (),
        focus::Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onAudioFocusChange
            as *const (),
        network::Java_com_alisonjenkins_pharos_auto_PharosNetworkCallback_onAvailable as *const (),
        network::Java_com_alisonjenkins_pharos_auto_PharosNetworkCallback_onLost as *const (),
        noisy::Java_com_alisonjenkins_pharos_auto_PharosNoisyReceiver_onReceive as *const (),
        player::Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onCompletion as *const (),
        player::Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onError as *const (),
        player::Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onPrepared as *const (),
        search::Java_com_alisonjenkins_pharos_auto_PharosMediaService_onSearch as *const (),
        search::Java_com_alisonjenkins_pharos_auto_PharosSearchActivity_nativeOnCreate as *const (),
        search::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlayFromSearch
            as *const (),
        search::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepareFromSearch
            as *const (),
        service::Java_com_alisonjenkins_pharos_auto_PharosMediaService_nativeOnCreate as *const (),
        service::Java_com_alisonjenkins_pharos_auto_PharosMediaService_nativeOnDestroy as *const (),
        service::Java_com_alisonjenkins_pharos_auto_PharosMediaService_onGetRoot as *const (),
        service::Java_com_alisonjenkins_pharos_auto_PharosMediaService_onLoadChildren as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPause as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlay as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlayFromMediaId
            as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepare as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepareFromMediaId
            as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSeekTo as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSkipToNext as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSkipToPrevious
            as *const (),
        session::Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onStop as *const (),
    ]
}

/// Keeps every JNI export in the final library. Call it once from the
/// app's cdylib, for example from a `#[used]` static or its own export.
pub fn link() {
    std::hint::black_box(exports());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_lists_every_export_once() {
        let mut seen = exports().to_vec();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), exports().len());
        assert!(seen.iter().all(|p| !p.is_null()));
        link();
    }

    #[test]
    fn track_length_prefers_the_player_and_falls_back_to_the_server() {
        assert_eq!(track_length(57_887, 57_840), (57_887, "player"));
        assert_eq!(track_length(-1, 316_160), (316_160, "server"));
        assert_eq!(track_length(0, 0), (0, "server"));
    }

    #[test]
    fn a_node_runtime_becomes_the_item_duration() {
        let track = |ms| content::Node {
            id: "track/1".into(),
            title: "A Song".into(),
            subtitle: String::new(),
            browsable: false,
            playable: true,
            art_url: None,
            stream_url: None,
            duration_ms: ms,
            album: "An Album".into(),
        };
        let item = Item::of_node(track(22_253));
        assert_eq!(
            (item.duration_ms, item.album.as_str()),
            (22_253, "An Album")
        );
        assert_eq!(Item::of_node(track(u64::MAX)).duration_ms, 0);
    }
}
