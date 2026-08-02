//! Route libav's C logging into `tracing` instead of the process's stderr.
//!
//! libav writes decoder/demuxer diagnostics straight to fd 2, which in the
//! crash-isolated `transcode-worker` is inherited to the pod's stdout. Those
//! lines are unstructured, carry no job/file identity, and are emitted per
//! frame: a library holding damaged or zero-length sources (see the corrupt
//! -media backlog) produces them continuously at `AV_LOG_ERROR`, so capping
//! the level does not touch them. During the B108 investigation the resulting
//! ~11.5M lines/hour evicted the structured JSON logs from both `kubectl logs`
//! and Loki — the observability was destroyed by the noise, not by a gap.
//!
//! Installing our own callback moves those lines onto `tracing` under the
//! `libav` target, where the ordinary `EnvFilter` governs them: at the default
//! `info` they cost one counter increment and nothing else. The text is not
//! lost — `PHAROS_LOG=info,libav=debug` brings it straight back, now as
//! structured records that interleave correctly with everything around them.
//!
//! Volume stays queryable regardless, because dropping the text without
//! leaving a signal behind is how a decoding problem becomes invisible:
//! `pharos_libav_log_total{level,component}` counts every message libav
//! offered, whether or not it was rendered.
//!
//! Note this replaces libav's default callback, and with it the
//! `AV_LOG_SKIP_REPEATED` collapsing that lives inside it. Consecutive
//! identical lines are no longer folded into "Last message repeated N times";
//! at the default filter they are not printed at all, and when the target is
//! enabled deliberately, the repetition is the thing being asked for.

use ffmpeg_the_third as ffmpeg;

use ffmpeg::ffi;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Longest formatted line kept, matching libav's own default callback so we
/// truncate exactly where it would.
const LINE_MAX: usize = 1024;

/// Longest component name accepted as a metric label. libav class names are
/// short and stable (`h264`, `matroska,webm`); anything longer is not one.
const COMPONENT_MAX: usize = 32;

/// Messages the bridge has been handed, before any level filtering.
///
/// The metric is the operational signal; this exists so a test can prove the
/// callback is actually installed and receiving — disarm [`install`] and it
/// stays at zero.
pub(crate) static RECEIVED: AtomicU64 = AtomicU64::new(0);

/// Whether an [`ErrorCapture`] is currently open. Read on every log line, so
/// it is an atomic rather than a lock.
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// Error-or-worse lines seen while a capture is open.
static CAPTURED: AtomicU64 = AtomicU64::new(0);
/// The first such line, kept verbatim.
static FIRST: Mutex<Option<String>> = Mutex::new(None);

/// Serialises captures. Only one may be open at a time, because libav's log
/// callback is process-global and has no way to attribute a line to a caller.
fn capture_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Counts the `AV_LOG_ERROR`-and-worse lines libav emits for the duration of
/// the guard's life, and keeps the first verbatim.
///
/// This exists because **a damaged container does not fail an API call**. The
/// matroska demuxer resyncs past a hole of null bytes internally: `av_read_frame`
/// returns no error, no packet carries `AV_PKT_FLAG_CORRUPT`, and the scan runs
/// to EOF reporting success — while `ffmpeg -v error … -c copy -f null -` on the
/// same file prints `0x00 at pos 77311 invalid as first byte of an EBML number`.
/// The only place the fault is stated is the log, so that is where an integrity
/// check has to read it. Measured, not assumed: an earlier version of this scan
/// counted only API-level errors and called a file with a 64 KiB hole clean.
///
/// Holding the guard blocks any other capture, so the count belongs to exactly
/// one scan. The `transcode-worker` runs one op at a time, so this never
/// contends in production; it matters for tests and for any future in-process
/// caller.
pub(crate) struct ErrorCapture {
    _guard: MutexGuard<'static, ()>,
}

impl ErrorCapture {
    pub(crate) fn open() -> Self {
        let guard = capture_lock().lock().unwrap_or_else(|e| e.into_inner());
        CAPTURED.store(0, Ordering::Relaxed);
        if let Ok(mut first) = FIRST.lock() {
            *first = None;
        }
        CAPTURING.store(true, Ordering::SeqCst);
        Self { _guard: guard }
    }

    /// Errors so far. Atomic-only, cheap enough to poll per packet — which is
    /// how the scan learns *where* in the media a logged fault happened, the
    /// log line itself carrying no timestamp.
    pub(crate) fn count(&self) -> u64 {
        CAPTURED.load(Ordering::Relaxed)
    }

    /// Errors so far, and the first message.
    pub(crate) fn errors(&self) -> (u64, Option<String>) {
        let n = CAPTURED.load(Ordering::Relaxed);
        let first = FIRST.lock().ok().and_then(|f| f.clone());
        (n, first)
    }
}

impl Drop for ErrorCapture {
    fn drop(&mut self) {
        CAPTURING.store(false, Ordering::SeqCst);
    }
}

/// Point libav's global log callback at [`on_log`]. Process-wide and not
/// undoable; call once.
pub(crate) fn install() {
    // SAFETY: `on_log` has the exact signature libav requires, is `extern "C"`,
    // unwinds nowhere, and stays valid for the life of the process.
    unsafe { ffi::av_log_set_callback(Some(on_log)) };
}

/// libav's callback. Formats the message the same way libav would, then hands
/// it to `tracing`.
///
/// # Safety
///
/// Called by libav with a valid format string and matching `va_list`. The
/// `va_list` is consumed exactly once, by `av_log_format_line2`.
unsafe extern "C" fn on_log(
    avcl: *mut c_void,
    level: c_int,
    fmt: *const c_char,
    // Not `ffi::va_list`: on this target that alias is `[__va_list_tag; 1]`,
    // while `av_log_set_callback` and `av_log_format_line2` both take the
    // decayed pointer form.
    vl: *mut ffi::__va_list_tag,
) {
    RECEIVED.fetch_add(1, Ordering::Relaxed);

    // An open capture wants every error line regardless of the global level,
    // so it cannot be starved by a quieter filter.
    let captured = CAPTURING.load(Ordering::SeqCst) && level <= ffi::AV_LOG_ERROR as c_int;

    // The level gate lives in libav's *default* callback, not in `av_vlog`, so
    // a custom callback receives everything and must filter for itself.
    // SAFETY: no arguments; safe at any time.
    if !captured && level > unsafe { ffi::av_log_get_level() } {
        return;
    }

    let mut buf = [0 as c_char; LINE_MAX];
    let mut print_prefix: c_int = 1;
    // SAFETY: `buf` is `LINE_MAX` writable bytes; `fmt`/`vl` come from libav
    // and are consumed once here.
    let written = unsafe {
        ffi::av_log_format_line2(
            avcl,
            level,
            fmt,
            vl,
            buf.as_mut_ptr(),
            LINE_MAX as c_int,
            &mut print_prefix,
        )
    };
    if written <= 0 {
        return;
    }

    // SAFETY: `av_log_format_line2` NUL-terminates within `LINE_MAX`.
    let line = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    let (component, message) = split_component(line.trim_end());

    if captured {
        CAPTURED.fetch_add(1, Ordering::Relaxed);
        // `try_lock`: this callback runs on libav's own threads and must never
        // block them. Losing the text of a line under contention costs a
        // slightly less specific reason, never the count or the verdict.
        if let Ok(mut first) = FIRST.try_lock() {
            if first.is_none() {
                *first = Some(format!("[{component}] {message}"));
            }
        }
    }

    metrics::counter!(
        "pharos_libav_log_total",
        "level" => level_label(level),
        "component" => component.to_owned(),
    )
    .increment(1);

    // Fatal and above mean libav is about to stop being useful, and those are
    // rare enough to always be worth a line. Everything else — including
    // `AV_LOG_ERROR`, which a single damaged source emits per frame — is
    // detail behind the counter.
    if level <= ffi::AV_LOG_FATAL as c_int {
        tracing::error!(target: "libav", component, "{message}");
    } else {
        tracing::debug!(target: "libav", component, "{message}");
    }
}

/// Stable, bounded metric label for a libav log level.
fn level_label(level: c_int) -> &'static str {
    match level {
        l if l <= ffi::AV_LOG_PANIC as c_int => "panic",
        l if l <= ffi::AV_LOG_FATAL as c_int => "fatal",
        l if l <= ffi::AV_LOG_ERROR as c_int => "error",
        l if l <= ffi::AV_LOG_WARNING as c_int => "warning",
        l if l <= ffi::AV_LOG_INFO as c_int => "info",
        l if l <= ffi::AV_LOG_VERBOSE as c_int => "verbose",
        l if l <= ffi::AV_LOG_DEBUG as c_int => "debug",
        _ => "trace",
    }
}

/// Split libav's `[h264 @ 0x7f..] message` prefix into a component name and
/// the message.
///
/// The pointer is deliberately dropped: it is a fresh heap address per decoder
/// instance, so keeping it makes every otherwise-identical line unique and
/// defeats both log dedup and the metric's cardinality bound.
///
/// Unrecognised shapes yield `("unknown", line)`, and names outside the
/// characters libav uses for class names yield `"other"` — a metric label is a
/// dashboard contract, and an unbounded one silently breaks it.
fn split_component(line: &str) -> (&str, &str) {
    let Some(rest) = line.strip_prefix('[') else {
        return ("unknown", line);
    };
    let Some((prefix, message)) = rest.split_once("] ") else {
        return ("unknown", line);
    };
    let name = prefix.split(" @ ").next().unwrap_or(prefix);
    if name.is_empty()
        || name.len() > COMPONENT_MAX
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b',' | b'+'))
    {
        return ("other", message);
    }
    (name, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn level_labels_are_distinct_and_stable() {
        let all = [
            ffi::AV_LOG_PANIC,
            ffi::AV_LOG_FATAL,
            ffi::AV_LOG_ERROR,
            ffi::AV_LOG_WARNING,
            ffi::AV_LOG_INFO,
            ffi::AV_LOG_VERBOSE,
            ffi::AV_LOG_DEBUG,
            ffi::AV_LOG_TRACE,
        ];
        let labels: Vec<&str> = all.iter().map(|l| level_label(*l as c_int)).collect();
        assert_eq!(
            labels,
            vec!["panic", "fatal", "error", "warning", "info", "verbose", "debug", "trace"],
            "metric labels are a dashboard contract; renaming one breaks alerts silently"
        );
    }

    #[test]
    fn component_is_split_off_and_the_pointer_dropped() {
        assert_eq!(
            split_component("[h264 @ 0x7ff9a00d4400] missing picture in access unit"),
            ("h264", "missing picture in access unit")
        );
        assert_eq!(
            split_component("[matroska,webm @ 0x7caa10000d80] EBML header parsing failed"),
            ("matroska,webm", "EBML header parsing failed")
        );
    }

    #[test]
    fn unrecognised_shapes_do_not_widen_the_label_set() {
        assert_eq!(
            split_component("Last message repeated 1 times"),
            ("unknown", "Last message repeated 1 times")
        );
        assert_eq!(
            split_component("[unterminated"),
            ("unknown", "[unterminated")
        );
        let long = "x".repeat(COMPONENT_MAX + 1);
        assert_eq!(
            split_component(&format!("[{long} @ 0x1] boom")),
            ("other", "boom")
        );
        assert_eq!(
            split_component("[weird name! @ 0x1] boom"),
            ("other", "boom")
        );
    }

    /// The bridge is only worth anything if libav actually routes through it.
    /// Disarm `install()` and this goes to zero.
    #[test]
    fn installed_callback_receives_libav_messages() {
        crate::libav::init().expect("libav init");
        let before = RECEIVED.load(Ordering::Relaxed);
        let msg = c"pharos logbridge probe\n";
        // SAFETY: null AVClass and a literal format string with no arguments.
        unsafe {
            ffi::av_log(
                std::ptr::null_mut(),
                ffi::AV_LOG_ERROR as c_int,
                msg.as_ptr(),
            )
        };
        assert!(
            RECEIVED.load(Ordering::Relaxed) > before,
            "libav log callback was not installed"
        );
    }
}
