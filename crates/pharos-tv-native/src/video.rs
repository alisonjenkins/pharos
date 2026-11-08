//! Video probe (spec 032 slice 0, player step 1): decode a file with the
//! TV's hardware decoder straight into the hole's Surface, so SurfaceFlinger
//! can put the frames on the display's video plane without a GPU copy.
//!
//! `setprop debug.pharos.video <path>` with `debug.pharos.hole 3` plays the
//! file's first video track in a loop, paced to its timestamps. The file
//! must be readable by the app (its external files dir works with adb push).
//! Logs the decoder's name and, every 2 s, frames shown and dropped.

use ndk::native_window::NativeWindow;
use ndk_sys as sys;
use std::ffi::CStr;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

pub fn path() -> Option<String> {
    android_properties::getprop("debug.pharos.video")
        .value()
        .filter(|p| p.starts_with('/'))
}

/// One poll of the decoder's queues.
const DEQUEUE_TIMEOUT_US: i64 = 2_000;
/// A frame later than this is released unshown, as a player would drop it.
const LATE: Duration = Duration::from_millis(40);
/// How often the frame counts are logged.
const REPORT_EVERY: Duration = Duration::from_secs(2);

pub fn play(window: NativeWindow, path: String) {
    let spawned = std::thread::Builder::new()
        .name("pharos-video".into())
        .spawn(move || {
            if let Err(e) = run(&window, &path) {
                log::warn!("video: {e} path={path}");
            }
        });
    if let Err(e) = spawned {
        log::warn!("video: thread: {e}");
    }
}

fn now_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out pointer. CLOCK_MONOTONIC is the clock
    // `releaseOutputBufferAtTime` expects (System.nanoTime).
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

fn check(status: sys::media_status_t, what: &str) -> Result<(), String> {
    if status == sys::media_status_t::AMEDIA_OK {
        Ok(())
    } else {
        Err(format!("{what}: media_status {}", status.0))
    }
}

fn run(window: &NativeWindow, path: &str) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let len = file.metadata().map_err(|e| format!("stat: {e}"))?.len() as i64;
    // SAFETY: every pointer below comes from the NDK call before it and is
    // checked for null; the extractor and codec live until this returns and
    // are deleted on the way out. `file` outlives the extractor.
    unsafe {
        let ex = sys::AMediaExtractor_new();
        if ex.is_null() {
            return Err("AMediaExtractor_new returned null".into());
        }
        let _ex_guard = Guard(move || {
            sys::AMediaExtractor_delete(ex);
        });
        check(
            sys::AMediaExtractor_setDataSourceFd(ex, file.as_raw_fd(), 0, len),
            "setDataSourceFd",
        )?;
        let tracks = sys::AMediaExtractor_getTrackCount(ex);
        let mut picked = None;
        for i in 0..tracks {
            let format = sys::AMediaExtractor_getTrackFormat(ex, i);
            let mut mime = std::ptr::null();
            if sys::AMediaFormat_getString(format, sys::AMEDIAFORMAT_KEY_MIME, &mut mime) {
                let m = CStr::from_ptr(mime).to_string_lossy().into_owned();
                if m.starts_with("video/") {
                    picked = Some((i, format, m));
                    break;
                }
            }
            sys::AMediaFormat_delete(format);
        }
        let (track, format, mime) = picked.ok_or("no video track")?;
        let _format_guard = Guard(move || {
            sys::AMediaFormat_delete(format);
        });
        check(sys::AMediaExtractor_selectTrack(ex, track), "selectTrack")?;
        let mime_c = std::ffi::CString::new(mime.clone()).map_err(|e| format!("mime: {e}"))?;
        let codec = sys::AMediaCodec_createDecoderByType(mime_c.as_ptr());
        if codec.is_null() {
            return Err(format!("no decoder for {mime}"));
        }
        let _codec_guard = Guard(move || {
            sys::AMediaCodec_stop(codec);
            sys::AMediaCodec_delete(codec);
        });
        let mut name = std::ptr::null_mut();
        let decoder =
            if sys::AMediaCodec_getName(codec, &mut name) == sys::media_status_t::AMEDIA_OK {
                let n = CStr::from_ptr(name).to_string_lossy().into_owned();
                sys::AMediaCodec_releaseName(codec, name);
                n
            } else {
                "?".to_owned()
            };
        let fmt = CStr::from_ptr(sys::AMediaFormat_toString(format))
            .to_string_lossy()
            .into_owned();
        log::info!("video: decoder={decoder} mime={mime} format={fmt}");
        check(
            sys::AMediaCodec_configure(
                codec,
                format,
                window.ptr().as_ptr(),
                std::ptr::null_mut(),
                0,
            ),
            "configure",
        )?;
        check(sys::AMediaCodec_start(codec), "start")?;

        let mut base_ns = now_ns();
        let mut first_pts: Option<i64> = None;
        let (mut shown, mut dropped, mut loops) = (0u64, 0u64, 0u64);
        let mut input_done = false;
        let mut report = Instant::now();
        loop {
            if !input_done {
                let idx = sys::AMediaCodec_dequeueInputBuffer(codec, DEQUEUE_TIMEOUT_US);
                if idx >= 0 {
                    let idx = idx as usize;
                    let mut cap = 0usize;
                    let buf = sys::AMediaCodec_getInputBuffer(codec, idx, &mut cap);
                    let n = sys::AMediaExtractor_readSampleData(ex, buf, cap);
                    if n < 0 {
                        sys::AMediaCodec_queueInputBuffer(
                            codec,
                            idx,
                            0,
                            0,
                            0,
                            sys::AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM as u32,
                        );
                        input_done = true;
                    } else {
                        let pts = sys::AMediaExtractor_getSampleTime(ex);
                        sys::AMediaCodec_queueInputBuffer(codec, idx, 0, n as usize, pts as u64, 0);
                        sys::AMediaExtractor_advance(ex);
                    }
                }
            }
            let mut info = sys::AMediaCodecBufferInfo {
                offset: 0,
                size: 0,
                presentationTimeUs: 0,
                flags: 0,
            };
            let idx = sys::AMediaCodec_dequeueOutputBuffer(codec, &mut info, DEQUEUE_TIMEOUT_US);
            if idx >= 0 {
                let idx = idx as usize;
                if info.flags & sys::AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM as u32 != 0 {
                    sys::AMediaCodec_releaseOutputBuffer(codec, idx, false);
                    loops += 1;
                    sys::AMediaExtractor_seekTo(
                        ex,
                        0,
                        sys::SeekMode::AMEDIAEXTRACTOR_SEEK_PREVIOUS_SYNC,
                    );
                    check(sys::AMediaCodec_flush(codec), "flush")?;
                    input_done = false;
                    first_pts = None;
                    continue;
                }
                let pts = info.presentationTimeUs;
                let first = *first_pts.get_or_insert_with(|| {
                    base_ns = now_ns() + 50_000_000;
                    pts
                });
                let due = base_ns + (pts - first) * 1_000;
                if now_ns() - due > LATE.as_nanos() as i64 {
                    sys::AMediaCodec_releaseOutputBuffer(codec, idx, false);
                    dropped += 1;
                } else {
                    sys::AMediaCodec_releaseOutputBufferAtTime(codec, idx, due);
                    shown += 1;
                }
            }
            if report.elapsed() >= REPORT_EVERY {
                log::info!("bench video shown={shown} dropped={dropped} loops={loops}");
                report = Instant::now();
            }
        }
    }
}

/// Runs its closure on drop, for the NDK handles above.
struct Guard<F: FnMut()>(F);

impl<F: FnMut()> Drop for Guard<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
