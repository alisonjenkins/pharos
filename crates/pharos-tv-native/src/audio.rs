//! Audio (spec 032 slice 3, player step 5): decode the stream's AAC with
//! `AMediaCodec` and play the PCM through AAudio, which also gives the clock
//! the video is paced to.
//!
//! AAudio reports which frame left the speaker at which `CLOCK_MONOTONIC`
//! time. With the PTS of the first frame written, that maps every media time
//! to a monotonic time, and the video releases each picture at its mapped
//! time, so picture follows sound whatever the output latency is.
//!
//! `setprop debug.pharos.gain <0..1>` scales the samples (default 0.25, a
//! quiet first try); `debug.pharos.audio off` leaves the audio out.

use crate::feeder::AudioTrack;
use ndk::audio::{
    AudioContentType, AudioDirection, AudioFormat, AudioStream, AudioStreamBuilder, AudioUsage,
    Clockid,
};
use ndk_sys as sys;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

/// Monotonic time minus media time, in ns, once AAudio has reported a
/// timestamp.
static OFFSET_NS: AtomicI64 = AtomicI64::new(0);
static READY: AtomicBool = AtomicBool::new(false);
/// Whether an audio player was started for the current stream.
static EXPECTED: AtomicBool = AtomicBool::new(false);

/// The monotonic time, in ns, at which media time `pts_us` is heard; `None`
/// before the audio clock runs.
pub fn due_ns(pts_us: i64) -> Option<i64> {
    READY
        .load(Ordering::Acquire)
        .then(|| pts_us * 1_000 + OFFSET_NS.load(Ordering::Acquire))
}

/// Waits up to `wait` for the audio clock, if audio was started.
pub fn wait_ready(wait: Duration) -> bool {
    if !EXPECTED.load(Ordering::Acquire) {
        return false;
    }
    let until = Instant::now() + wait;
    while Instant::now() < until {
        if READY.load(Ordering::Acquire) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

pub fn enabled() -> bool {
    android_properties::getprop("debug.pharos.audio")
        .value()
        .is_none_or(|v| v != "off")
}

fn gain() -> f32 {
    android_properties::getprop("debug.pharos.gain")
        .value()
        .and_then(|v| v.parse::<f32>().ok())
        .map_or(0.25, |g| g.clamp(0.0, 1.0))
}

pub fn play(track: AudioTrack) {
    EXPECTED.store(true, Ordering::Release);
    READY.store(false, Ordering::Release);
    let spawned = std::thread::Builder::new()
        .name("pharos-audio".into())
        .spawn(move || {
            if let Err(e) = run(track) {
                log::warn!("audio: {e}");
            }
            EXPECTED.store(false, Ordering::Release);
        });
    if let Err(e) = spawned {
        log::warn!("audio: thread: {e}");
        EXPECTED.store(false, Ordering::Release);
    }
}

const DEQUEUE_TIMEOUT_US: i64 = 5_000;
const REPORT_EVERY: Duration = Duration::from_secs(2);

fn check(status: sys::media_status_t, what: &str) -> Result<(), String> {
    if status == sys::media_status_t::AMEDIA_OK {
        Ok(())
    } else {
        Err(format!("{what}: media_status {}", status.0))
    }
}

/// The frames to decode, in order: the first segment's, then each later one's.
struct Frames {
    current: std::vec::IntoIter<crate::ts::AccessUnit>,
    rest: std::sync::mpsc::Receiver<Vec<crate::ts::AccessUnit>>,
}

impl Frames {
    /// The next frame, waiting for the demux thread; `None` at the end.
    fn next(&mut self) -> Option<crate::ts::AccessUnit> {
        loop {
            if let Some(f) = self.current.next() {
                return Some(f);
            }
            self.current = self.rest.recv().ok()?.into_iter();
        }
    }
}

fn run(track: AudioTrack) -> Result<(), String> {
    let config = track.config;
    let mut frames = Frames {
        current: track.first.into_iter(),
        rest: track.rest,
    };
    let gain = gain();
    log::info!(
        "audio: aac object={} {} Hz {} ch gain={gain}",
        config.object_type,
        config.sample_rate,
        config.channels
    );
    // SAFETY: every pointer comes from the NDK call before it and is checked
    // for null; the format and codec are deleted on the way out.
    unsafe {
        let format = sys::AMediaFormat_new();
        if format.is_null() {
            return Err("AMediaFormat_new returned null".into());
        }
        let _format_guard = Guard(move || {
            sys::AMediaFormat_delete(format);
        });
        let mime = c"audio/mp4a-latm";
        sys::AMediaFormat_setString(format, sys::AMEDIAFORMAT_KEY_MIME, mime.as_ptr());
        sys::AMediaFormat_setInt32(
            format,
            sys::AMEDIAFORMAT_KEY_SAMPLE_RATE,
            i32::try_from(config.sample_rate).unwrap_or(48_000),
        );
        sys::AMediaFormat_setInt32(
            format,
            sys::AMEDIAFORMAT_KEY_CHANNEL_COUNT,
            i32::from(config.channels),
        );
        let asc = config.audio_specific_config();
        sys::AMediaFormat_setBuffer(format, c"csd-0".as_ptr(), asc.as_ptr().cast(), asc.len());
        let codec = sys::AMediaCodec_createDecoderByType(mime.as_ptr());
        if codec.is_null() {
            return Err("no AAC decoder".into());
        }
        let _codec_guard = Guard(move || {
            sys::AMediaCodec_stop(codec);
            sys::AMediaCodec_delete(codec);
        });
        check(
            sys::AMediaCodec_configure(
                codec,
                format,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            ),
            "configure",
        )?;
        check(sys::AMediaCodec_start(codec), "start")?;

        let mut out = Output::default();
        let mut input_done = false;
        let mut report = Instant::now();
        loop {
            if !input_done {
                input_done = !feed(codec, &mut frames);
            }
            let mut info = sys::AMediaCodecBufferInfo {
                offset: 0,
                size: 0,
                presentationTimeUs: 0,
                flags: 0,
            };
            let idx = sys::AMediaCodec_dequeueOutputBuffer(codec, &mut info, DEQUEUE_TIMEOUT_US);
            if idx == sys::AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED as isize {
                let f = sys::AMediaCodec_getOutputFormat(codec);
                let (mut rate, mut channels) = (0, 0);
                sys::AMediaFormat_getInt32(f, sys::AMEDIAFORMAT_KEY_SAMPLE_RATE, &mut rate);
                sys::AMediaFormat_getInt32(f, sys::AMEDIAFORMAT_KEY_CHANNEL_COUNT, &mut channels);
                sys::AMediaFormat_delete(f);
                log::info!("audio: decoder output {rate} Hz {channels} ch");
                out.rate = rate;
                out.channels = channels;
                continue;
            }
            if idx >= 0 {
                let idx = idx as usize;
                if info.flags & sys::AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM as u32 != 0 {
                    sys::AMediaCodec_releaseOutputBuffer(codec, idx, false);
                    log::info!("bench audio end written={}", out.written);
                    return Ok(());
                }
                let mut size = 0usize;
                let buf = sys::AMediaCodec_getOutputBuffer(codec, idx, &mut size);
                if !buf.is_null() && info.size > 0 {
                    let start = buf.add(info.offset as usize).cast::<i16>();
                    let samples = std::slice::from_raw_parts(start, info.size as usize / 2);
                    out.write(samples, info.presentationTimeUs, gain)?;
                }
                sys::AMediaCodec_releaseOutputBuffer(codec, idx, false);
            }
            if report.elapsed() >= REPORT_EVERY {
                out.report();
                report = Instant::now();
            }
        }
    }
}

/// Queues the next AAC frame, without its ADTS header. False once the end
/// of stream is queued.
///
/// # Safety
/// `codec` is a started AAC decoder.
unsafe fn feed(codec: *mut sys::AMediaCodec, frames: &mut Frames) -> bool {
    let idx = sys::AMediaCodec_dequeueInputBuffer(codec, DEQUEUE_TIMEOUT_US);
    if idx < 0 {
        return true;
    }
    let idx = idx as usize;
    let mut cap = 0usize;
    let buf = sys::AMediaCodec_getInputBuffer(codec, idx, &mut cap);
    let eos = sys::AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM as u32;
    let Some(frame) = frames.next() else {
        sys::AMediaCodec_queueInputBuffer(codec, idx, 0, 0, 0, eos);
        return false;
    };
    let raw = crate::ts::adts_payload(&frame.data);
    let len = raw.len().min(cap);
    if !buf.is_null() {
        std::ptr::copy_nonoverlapping(raw.as_ptr(), buf, len);
    }
    sys::AMediaCodec_queueInputBuffer(codec, idx, 0, len, frame.pts_us, 0);
    true
}

/// The AAudio stream, opened on the decoder's first PCM.
#[derive(Default)]
struct Output {
    stream: Option<AudioStream>,
    rate: i32,
    channels: i32,
    /// PTS of the first frame written, in µs.
    first_pts: Option<i64>,
    written: i64,
    scratch: Vec<i16>,
}

impl Output {
    fn write(&mut self, samples: &[i16], pts_us: i64, gain: f32) -> Result<(), String> {
        if self.rate <= 0 || self.channels <= 0 {
            return Err("PCM before the decoder's output format".into());
        }
        if self.stream.is_none() {
            let stream = AudioStreamBuilder::new()
                .map_err(|e| format!("aaudio builder: {e}"))?
                .direction(AudioDirection::Output)
                .format(AudioFormat::PCM_I16)
                .sample_rate(self.rate)
                .channel_count(self.channels)
                .usage(AudioUsage::Media)
                .content_type(AudioContentType::Movie)
                .open_stream()
                .map_err(|e| format!("aaudio open: {e}"))?;
            stream.request_start().map_err(|e| format!("aaudio start: {e}"))?;
            log::info!(
                "audio: aaudio {} Hz {} ch burst={} buffer={}",
                stream.sample_rate(),
                stream.channel_count(),
                stream.frames_per_burst(),
                stream.buffer_size_in_frames()
            );
            self.stream = Some(stream);
            self.first_pts = Some(pts_us);
        }
        self.scratch.clear();
        self.scratch
            .extend(samples.iter().map(|&s| (f32::from(s) * gain) as i16));
        let frames = i32::try_from(self.scratch.len() / self.channels as usize).unwrap_or(0);
        let Some(stream) = &self.stream else {
            return Ok(());
        };
        // SAFETY: `scratch` holds `frames` interleaved i16 frames.
        let n = unsafe { stream.write(self.scratch.as_ptr().cast(), frames, 1_000_000_000) }
            .map_err(|e| format!("aaudio write: {e}"))?;
        self.written += i64::from(n);
        self.publish_clock();
        Ok(())
    }

    /// Maps media time to monotonic time from AAudio's latest timestamp.
    fn publish_clock(&self) {
        let (Some(stream), Some(first)) = (&self.stream, self.first_pts) else {
            return;
        };
        let Ok(ts) = stream.timestamp(Clockid::Monotonic) else {
            return;
        };
        let media_ns = first * 1_000 + ts.frame_position * 1_000_000_000 / i64::from(self.rate);
        OFFSET_NS.store(ts.time_nanoseconds - media_ns, Ordering::Release);
        if !READY.swap(true, Ordering::AcqRel) {
            log::info!("audio: clock ready at frame {}", ts.frame_position);
        }
    }

    fn report(&self) {
        let xruns = self.stream.as_ref().map_or(0, AudioStream::x_run_count);
        log::info!(
            "bench audio written={} xruns={xruns} offset_ms={}",
            self.written,
            OFFSET_NS.load(Ordering::Acquire) / 1_000_000
        );
    }
}

/// Runs its closure on drop, for the NDK handles above.
struct Guard<F: FnMut()>(F);

impl<F: FnMut()> Drop for Guard<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
