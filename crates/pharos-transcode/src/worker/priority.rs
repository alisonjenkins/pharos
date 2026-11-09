//! Background ops at the lowest CPU priority (B908, V856).
//!
//! The libav worker runs trickplay sheets for the whole library beside the
//! probes and posters a viewer waits on, and the server's ffmpeg encodes run
//! beside both. A sheet is never urgent, so its decode runs at `nice` 19 and
//! yields the CPU to playback whenever the two compete.
//!
//! On Linux `nice` belongs to a thread, and threads it starts (libav's decode
//! threads) inherit it, so the op gets its own thread: the priority covers the
//! whole op and goes away with the thread, and the worker's other ops keep
//! theirs. An unprivileged thread can lower its priority but never raise it
//! back, which is why the thread is not reused. Elsewhere `setpriority`
//! names the whole process, so the op runs at the normal priority.

/// The `nice` a background op runs at: the lowest priority there is.
pub const BACKGROUND_NICENESS: i32 = 19;

/// What a lowest-priority op got: `f`'s result and the `nice` it ran at,
/// or the error that left it at the inherited one.
#[derive(Debug)]
pub struct Ran<T> {
    pub value: T,
    pub niceness: Result<i32, String>,
}

/// Start `f` on a new thread at [`BACKGROUND_NICENESS`]; the receiver gets
/// its result, or the panic's message. `Err` when no thread could start.
pub fn spawn_at_lowest_priority<T, F>(
    f: F,
) -> std::io::Result<tokio::sync::oneshot::Receiver<Result<Ran<T>, String>>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("bg-lowest".into())
        .spawn(move || {
            let niceness = lower_this_thread();
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .map(|value| Ran { value, niceness })
                .map_err(|p| panic_message(p.as_ref()));
            let _ = tx.send(ran);
        })?;
    Ok(rx)
}

/// [`spawn_at_lowest_priority`], waited for on this thread.
pub fn run_at_lowest_priority<T, F>(f: F) -> Result<Ran<T>, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    spawn_at_lowest_priority(f)
        .map_err(|e| format!("spawn op thread: {e}"))?
        .blocking_recv()
        .map_err(|_| "op thread ended without an answer".to_string())?
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic".into())
}

/// This thread's `nice`, on Linux.
#[cfg(target_os = "linux")]
pub fn this_thread_niceness() -> Result<i32, String> {
    // SAFETY: gettid and getpriority are plain syscalls with no pointers.
    // getpriority returns -1 for both a nice of -1 and an error, so errno is
    // cleared first and read after.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        *libc::__errno_location() = 0;
        let n = libc::getpriority(libc::PRIO_PROCESS, tid);
        if n == -1 && *libc::__errno_location() != 0 {
            return Err(format!("getpriority: {}", std::io::Error::last_os_error()));
        }
        Ok(n)
    }
}

#[cfg(target_os = "linux")]
fn lower_this_thread() -> Result<i32, String> {
    // SAFETY: as above; setpriority on this thread's own id changes only it.
    let rc = unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, BACKGROUND_NICENESS)
    };
    if rc != 0 {
        return Err(format!(
            "setpriority {BACKGROUND_NICENESS}: {}",
            std::io::Error::last_os_error()
        ));
    }
    this_thread_niceness()
}

#[cfg(not(target_os = "linux"))]
fn lower_this_thread() -> Result<i32, String> {
    Err("per-thread nice needs Linux".into())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_op_runs_at_the_lowest_priority_and_the_caller_keeps_its_own() {
        let before = this_thread_niceness().expect("caller niceness");
        let ran = run_at_lowest_priority(|| this_thread_niceness().expect("op niceness"))
            .expect("op thread");
        assert_eq!(ran.value, BACKGROUND_NICENESS);
        assert_eq!(ran.niceness, Ok(BACKGROUND_NICENESS));
        assert_eq!(this_thread_niceness().expect("caller niceness"), before);
    }

    #[test]
    fn threads_the_op_starts_inherit_its_priority() {
        let ran = run_at_lowest_priority(|| {
            std::thread::spawn(|| this_thread_niceness().expect("child niceness"))
                .join()
                .expect("child")
        })
        .expect("op thread");
        assert_eq!(
            ran.value, BACKGROUND_NICENESS,
            "libav's decode threads run at 19 too"
        );
    }

    #[test]
    fn a_panic_in_the_op_comes_back_with_its_message() {
        let err = run_at_lowest_priority(|| -> () { panic!("boom {}", 7) }).unwrap_err();
        assert_eq!(err, "boom 7");
    }
}
