//! Video probe (spec 032 slice 0, player step 1): decode a file with the
//! TV's hardware decoder straight into the hole's Surface, so SurfaceFlinger
//! can put the frames on the display's video plane without a GPU copy.
//!
//! `setprop debug.pharos.video <path>` with `debug.pharos.hole 3` plays the
//! file's first video track in a loop, paced to its timestamps. The file
//! must be readable by the app (its external files dir works with adb push).
//! Logs the decoder's name and, every 2 s, frames shown and dropped.
//!
//! A directory holding an fMP4 HLS `index.m3u8` is played through the
//! `pharos-hls` feeder (src/feeder.rs) instead of the extractor.

use ndk::native_window::NativeWindow;
use ndk_sys as sys;
use std::ffi::CStr;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

pub fn path() -> Option<String> {
    android_properties::getprop("debug.pharos.video")
        .value()
        .filter(|p| p.starts_with('/') || p.starts_with(PLAY))
}

/// `debug.pharos.video play:<item id>` plays that item from the server.
const PLAY: &str = "play:";

/// The item `debug.pharos.video` asks to play from the server, if any.
pub fn play_item() -> Option<pharos_client::api::ItemRef> {
    path()?.strip_prefix(PLAY)?.trim().parse().ok()
}

/// Whether the video fills the window (a server playback) rather than the
/// slice 0 probe's rectangle.
pub fn full_window() -> bool {
    path().is_some_and(|p| p.starts_with(PLAY))
}

static STREAM: std::sync::Mutex<Option<Result<String, String>>> = std::sync::Mutex::new(None);
static STREAM_READY: std::sync::Condvar = std::sync::Condvar::new();

/// The live connection hands over the decision's stream URL, or why there
/// is none.
pub fn set_stream(url: Result<String, String>) {
    if let Ok(mut slot) = STREAM.lock() {
        *slot = Some(url);
        STREAM_READY.notify_all();
    }
}

fn wait_stream() -> Result<String, String> {
    let mut slot = STREAM.lock().map_err(|_| "stream lock poisoned")?;
    loop {
        if let Some(url) = slot.take() {
            return url;
        }
        slot = STREAM_READY.wait(slot).map_err(|_| "stream lock poisoned")?;
    }
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

/// Where the decoder's input comes from.
enum Source {
    /// A file `AMediaExtractor` demuxes (the slice 0 probe).
    Extractor { ex: *mut sys::AMediaExtractor, _file: std::fs::File },
    /// An fMP4 HLS directory `pharos-hls` demuxes.
    Fmp4(crate::feeder::Feeder),
}

impl Source {
    /// Opens `path` and returns the source, its video format and its mime type.
    ///
    /// # Safety
    /// The returned format must be deleted with `AMediaFormat_delete`.
    unsafe fn open(path: &str) -> Result<(Self, *mut sys::AMediaFormat, String), String> {
        let from_server = path.starts_with(PLAY);
        if from_server || crate::feeder::Feeder::is_feeder_dir(path) {
            let mut feeder = if from_server {
                let url = wait_stream()?;
                let stream = crate::hls_net::open(&crate::live::server(), &url)?;
                log::info!("video: playing {} from the server", stream.variant);
                match stream.packaging {
                    crate::hls_net::Packaging::Fmp4 { init } => {
                        crate::feeder::Feeder::from_init(&init, stream.segments)?
                    }
                    crate::hls_net::Packaging::Ts { first, size } => {
                        crate::feeder::Feeder::from_ts(first, stream.segments, size)?
                    }
                }
            } else {
                crate::feeder::Feeder::open(path)?
            };
            if let Some(track) = feeder.take_audio() {
                if crate::audio::enabled() {
                    crate::audio::play(track);
                }
            }
            let f = feeder.format();
            let format = sys::AMediaFormat_new();
            if format.is_null() {
                return Err("AMediaFormat_new returned null".into());
            }
            let mime = f.codec.mime();
            let mime_c = std::ffi::CString::new(mime).map_err(|e| format!("mime: {e}"))?;
            sys::AMediaFormat_setString(format, sys::AMEDIAFORMAT_KEY_MIME, mime_c.as_ptr());
            sys::AMediaFormat_setInt32(format, sys::AMEDIAFORMAT_KEY_WIDTH, i32::from(f.width));
            sys::AMediaFormat_setInt32(format, sys::AMEDIAFORMAT_KEY_HEIGHT, i32::from(f.height));
            sys::AMediaFormat_setInt32(
                format,
                sys::AMEDIAFORMAT_KEY_MAX_INPUT_SIZE,
                i32::try_from(f.max_input).unwrap_or(i32::MAX),
            );
            for (i, csd) in f.csd.iter().enumerate() {
                let key = std::ffi::CString::new(format!("csd-{i}"))
                    .map_err(|e| format!("csd key: {e}"))?;
                sys::AMediaFormat_setBuffer(format, key.as_ptr(), csd.as_ptr().cast(), csd.len());
            }
            return Ok((Self::Fmp4(feeder), format, mime.to_owned()));
        }

        let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
        let len = file.metadata().map_err(|e| format!("stat: {e}"))?.len() as i64;
        let ex = sys::AMediaExtractor_new();
        if ex.is_null() {
            return Err("AMediaExtractor_new returned null".into());
        }
        let source = Self::Extractor { ex, _file: file };
        let Self::Extractor { _file: file, .. } = &source else {
            return Err("unreachable source".into());
        };
        check(
            sys::AMediaExtractor_setDataSourceFd(ex, file.as_raw_fd(), 0, len),
            "setDataSourceFd",
        )?;
        let tracks = sys::AMediaExtractor_getTrackCount(ex);
        for i in 0..tracks {
            let format = sys::AMediaExtractor_getTrackFormat(ex, i);
            let mut mime = std::ptr::null();
            if sys::AMediaFormat_getString(format, sys::AMEDIAFORMAT_KEY_MIME, &mut mime) {
                let m = CStr::from_ptr(mime).to_string_lossy().into_owned();
                if m.starts_with("video/") {
                    check(sys::AMediaExtractor_selectTrack(ex, i), "selectTrack")?;
                    return Ok((source, format, m));
                }
            }
            sys::AMediaFormat_delete(format);
        }
        Err("no video track".into())
    }

    /// Fills one input buffer. Returns false once the end of stream is queued.
    ///
    /// # Safety
    /// `codec` is a started decoder this source was configured for.
    unsafe fn feed(&mut self, codec: *mut sys::AMediaCodec) -> Result<bool, String> {
        let idx = sys::AMediaCodec_dequeueInputBuffer(codec, DEQUEUE_TIMEOUT_US);
        if idx < 0 {
            return Ok(true);
        }
        let idx = idx as usize;
        let mut cap = 0usize;
        let buf = sys::AMediaCodec_getInputBuffer(codec, idx, &mut cap);
        if buf.is_null() {
            return Err("getInputBuffer returned null".into());
        }
        let eos = sys::AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM as u32;
        match self {
            Self::Extractor { ex, .. } => {
                let n = sys::AMediaExtractor_readSampleData(*ex, buf, cap);
                if n < 0 {
                    sys::AMediaCodec_queueInputBuffer(codec, idx, 0, 0, 0, eos);
                    return Ok(false);
                }
                let pts = sys::AMediaExtractor_getSampleTime(*ex);
                sys::AMediaCodec_queueInputBuffer(codec, idx, 0, n as usize, pts as u64, 0);
                sys::AMediaExtractor_advance(*ex);
                Ok(true)
            }
            Self::Fmp4(feeder) => {
                let dst = std::slice::from_raw_parts_mut(buf, cap);
                match feeder.next(dst)? {
                    crate::feeder::Next::Sample { len, pts_us, key } => {
                        let flags = if key { BUFFER_FLAG_KEY_FRAME } else { 0 };
                        sys::AMediaCodec_queueInputBuffer(codec, idx, 0, len, pts_us, flags);
                        Ok(true)
                    }
                    crate::feeder::Next::TooBig { need } => {
                        Err(format!("sample of {need} bytes over input buffer of {cap}"))
                    }
                    crate::feeder::Next::End => {
                        sys::AMediaCodec_queueInputBuffer(codec, idx, 0, 0, 0, eos);
                        Ok(false)
                    }
                }
            }
        }
    }

    /// # Safety
    /// The extractor, if any, is live.
    unsafe fn rewind(&mut self) {
        match self {
            Self::Extractor { ex, .. } => {
                sys::AMediaExtractor_seekTo(*ex, 0, sys::SeekMode::AMEDIAEXTRACTOR_SEEK_PREVIOUS_SYNC);
            }
            Self::Fmp4(feeder) => feeder.rewind(),
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Self::Extractor { ex, .. } = self {
            // SAFETY: `ex` came from AMediaExtractor_new and is deleted once.
            unsafe { sys::AMediaExtractor_delete(*ex) };
        }
    }
}

/// MediaCodec.BUFFER_FLAG_KEY_FRAME; ndk-sys does not export it.
const BUFFER_FLAG_KEY_FRAME: u32 = 1;

fn run(window: &NativeWindow, path: &str) -> Result<(), String> {
    // SAFETY: every pointer below comes from the NDK call before it and is
    // checked for null; the source, format and codec live until this returns
    // and are deleted on the way out.
    unsafe {
        let (mut source, format, mime) = Source::open(path)?;
        let _format_guard = Guard(move || {
            sys::AMediaFormat_delete(format);
        });
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
        // Worst lateness of a decoded frame in this report window, to tell a
        // decoder or feeder stall (one long gap) from steady slowness.
        let mut late_max_ms = i64::MIN;
        let mut input_done = false;
        let mut report = Instant::now();
        loop {
            if !input_done {
                input_done = !source.feed(codec)?;
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
                    if path.starts_with(PLAY) {
                        log::info!("bench video end shown={shown} dropped={dropped}");
                        return Ok(());
                    }
                    loops += 1;
                    source.rewind();
                    check(sys::AMediaCodec_flush(codec), "flush")?;
                    input_done = false;
                    first_pts = None;
                    continue;
                }
                let pts = info.presentationTimeUs;
                let first = *first_pts.get_or_insert_with(|| {
                    // Picture follows sound: hold the first frame until the
                    // audio clock runs, if there is audio.
                    if crate::audio::wait_ready(Duration::from_secs(3)) {
                        log::info!("video: paced to the audio clock");
                    }
                    base_ns = now_ns() + 50_000_000;
                    pts
                });
                let due = crate::audio::due_ns(pts)
                    .unwrap_or(base_ns + (pts - first) * 1_000);
                let late = now_ns() - due;
                late_max_ms = late_max_ms.max(late / 1_000_000);
                if late > LATE.as_nanos() as i64 {
                    sys::AMediaCodec_releaseOutputBuffer(codec, idx, false);
                    dropped += 1;
                } else {
                    sys::AMediaCodec_releaseOutputBufferAtTime(codec, idx, due);
                    shown += 1;
                }
            }
            if report.elapsed() >= REPORT_EVERY {
                log::info!(
                    "bench video shown={shown} dropped={dropped} loops={loops} late_max_ms={late_max_ms}"
                );
                late_max_ms = i64::MIN;
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
