//! Per-subtitle-line audio signals and video keyframe times in ONE pass over
//! the file (spec 028, FR-009, FR-012).
//!
//! The input is opened once and demuxed once. Packets of two streams matter:
//!
//! - the chosen audio stream is decoded, reduced to one channel at
//!   [`ANALYSIS_RATE`] and fed to a [`SignalAnalyser`] (the front-centre
//!   channel when the layout has one, because dialogue sits there and music
//!   and effects mostly do not; otherwise a mono downmix);
//! - the best video stream's packets are inspected: a packet with the key
//!   flag contributes its timestamp, and a keyframe that a line's middle
//!   falls within [`TEXT_SAMPLE_MAX_AGE_MS`] after is decoded on its own,
//!   scaled to a small luma plane and checked for text burned into the bottom
//!   of the picture ([`crate::burned_text`], B1207). No other video is
//!   decoded.
//!
//! Audio decoding stops once the stream is past the last requested window,
//! but the demux carries on to the end, because the keyframes come from all of
//! it. That demux is the one read of the file (SC-003).
//!
//! Timestamps are the streams' own, in ms, unshifted, which is the timeline
//! `subtitle_windows` reports its windows on.

use super::frames::FrameError;
use crate::burned_text::bottom_text_share;
use crate::picture_area::LumaPlane;
use crate::subtitle_signals::{
    cuts_from_keyframes, place_frame, ChannelPick, LineSignals, SignalAnalyser, SubtitleSignals,
    ANALYSIS_RATE,
};
use ffmpeg::ffi;
use ffmpeg::format::Sample;
use ffmpeg::{codec, format, frame, media, software, Channel, ChannelLayout, ChannelOrder};
use ffmpeg_the_third as ffmpeg;
use std::collections::BTreeMap;
use std::path::Path;

/// Audio kept after the last window, so a window ending at a frame boundary
/// is complete.
const TAIL_MS: u64 = 1_000;
/// Consecutive demux read errors after which the pass stops.
const MAX_READ_ERRORS: u32 = 64;
/// Undecodable frames tolerated before the audio is abandoned.
const MAX_BAD_FRAMES: u32 = 200;
/// A keyframe speaks for the picture under a line whose middle is at most
/// this long after it (and before the next keyframe).
pub const TEXT_SAMPLE_MAX_AGE_MS: u64 = 4_000;
/// Width of the luma plane a keyframe is scaled to for the text check.
const TEXT_SAMPLE_WIDTH: u32 = 480;
/// Undecodable keyframes tolerated before the text check is abandoned.
const MAX_BAD_KEYFRAMES: u32 = 50;

struct TimeBase {
    num: i64,
    den: i64,
}

impl TimeBase {
    fn of(tb: ffmpeg::Rational) -> Self {
        TimeBase {
            num: i64::from(tb.numerator()),
            den: i64::from(tb.denominator()),
        }
    }

    /// `ts` in ms, negative when `ts` is. i128: a 1/1e9 time base times 1000
    /// overflows i64 past ~2.5 hours.
    fn ms(&self, ts: i64) -> i64 {
        if self.den == 0 {
            return 0;
        }
        let ms = i128::from(ts) * 1000 * i128::from(self.num) / i128::from(self.den);
        i64::try_from(ms).unwrap_or(if ms < 0 { i64::MIN } else { i64::MAX })
    }
}

/// Everything the audio half carries across packets.
struct AudioPass {
    stream_index: usize,
    tb: TimeBase,
    decoder: ffmpeg::decoder::Audio,
    resampler: software::resampling::Context,
    src_channels: u32,
    /// Plane of the resampler's output to read: the front-centre channel's, or
    /// 0 for the mono downmix.
    plane: usize,
    /// `plane` is the front-centre channel rather than a downmix.
    fc: bool,
    analyser: SignalAnalyser,
    /// Stop decoding once this many samples are in.
    sample_cap: u64,
    done: bool,
    bad_frames: u32,
}

impl AudioPass {
    /// Drain every decodable frame into the analyser.
    fn drain(&mut self) -> Result<(), FrameError> {
        let mut decoded = frame::Audio::empty();
        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    // SAFETY: `decoded` owns a valid AVFrame; force the native
                    // layout order the resampler was built with (see
                    // waveform.rs).
                    unsafe {
                        let p = decoded.as_mut_ptr();
                        ffi::av_channel_layout_uninit(&mut (*p).ch_layout);
                        ffi::av_channel_layout_default(
                            &mut (*p).ch_layout,
                            self.src_channels as i32,
                        );
                    }
                    let mut out = frame::Audio::empty();
                    if self.resampler.run(&decoded, &mut out).is_err() {
                        // As a damaged frame: skip it, give up past the cap.
                        self.bad_frames += 1;
                        if self.bad_frames > MAX_BAD_FRAMES {
                            self.done = true;
                            return Ok(());
                        }
                        continue;
                    }
                    let n = out.samples();
                    if n > 0 && self.plane < out.planes().max(1) {
                        let data: &[f32] = out.plane(self.plane);
                        let data = &data[..n.min(data.len())];
                        // Place by pts: fill a gap, drop what lies before
                        // t = 0 (negative start pts) or overlaps.
                        let (silence, skip) = match decoded.pts() {
                            Some(pts) => place_frame(
                                self.analyser.samples(),
                                self.tb.ms(pts),
                                data.len(),
                                self.sample_cap,
                            ),
                            None => (0, 0),
                        };
                        self.analyser.push_silence(silence);
                        self.analyser.push(&data[skip..]);
                    }
                    if self.analyser.samples() >= self.sample_cap {
                        self.done = true;
                        return Ok(());
                    }
                }
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => return Ok(()),
                Err(ffmpeg::Error::Eof) => return Ok(()),
                Err(_) => {
                    self.bad_frames += 1;
                    self.done = self.bad_frames > MAX_BAD_FRAMES;
                    return Ok(());
                }
            }
        }
    }

    fn packet(&mut self, packet: &ffmpeg::Packet) -> Result<(), FrameError> {
        if self.decoder.send_packet(packet).is_err() {
            self.bad_frames += 1;
            self.done = self.bad_frames > MAX_BAD_FRAMES;
            return Ok(());
        }
        self.drain()
    }
}

/// The text check's half: a decoder fed one key packet at a time, and what
/// each decoded keyframe showed.
///
/// A key packet is held until the next one arrives, because only then is it
/// known to be the latest keyframe before some line's middle; so each
/// keyframe that speaks for a line is decoded once and no other is.
struct TextPass {
    decoder: ffmpeg::decoder::Video,
    scaler: Option<(format::Pixel, u32, u32, software::scaling::Context)>,
    /// Line middles, ascending.
    mids: Vec<u64>,
    held: Option<(u64, ffmpeg::Packet)>,
    /// Bottom-text share per decoded keyframe, by its time in ms.
    shares: BTreeMap<u64, f32>,
    bad: u32,
}

impl TextPass {
    fn open(ictx: &format::context::Input, idx: usize, windows: &[(u64, u64)]) -> Option<Self> {
        let stream = ictx.stream(idx)?;
        let ctx = codec::context::Context::from_parameters(stream.parameters()).ok()?;
        let mut decoder = ctx.decoder().video().ok()?;
        decoder.skip_frame(ffmpeg::Discard::NonKey);
        // The text check needs strokes, not a clean picture.
        // SAFETY: `decoder` owns a valid, opened AVCodecContext.
        unsafe {
            (*decoder.as_mut_ptr()).skip_loop_filter = ffi::AVDiscard::from(ffmpeg::Discard::All);
        }
        let mut mids: Vec<u64> = windows.iter().map(|&(s, e)| s / 2 + e / 2).collect();
        mids.sort_unstable();
        Some(TextPass {
            decoder,
            scaler: None,
            mids,
            held: None,
            shares: BTreeMap::new(),
            bad: 0,
        })
    }

    /// Whether a keyframe at `t_ms` followed by the next at `next_ms`
    /// (`None`: no more) is the one some line's middle reads.
    fn speaks(&self, t_ms: u64, next_ms: Option<u64>) -> bool {
        let i = self.mids.partition_point(|&m| m < t_ms);
        self.mids
            .get(i)
            .is_some_and(|&m| m - t_ms <= TEXT_SAMPLE_MAX_AGE_MS && next_ms.map_or(true, |n| m < n))
    }

    fn give_up(&self) -> bool {
        self.bad > MAX_BAD_KEYFRAMES
    }

    /// A key packet at `t_ms`, in demux order.
    fn key_packet(&mut self, t_ms: u64, packet: &ffmpeg::Packet) {
        if self.give_up() {
            return;
        }
        if let Some((held_t, held)) = self.held.take() {
            if held_t < t_ms && self.speaks(held_t, Some(t_ms)) {
                self.decode(held_t, &held);
            }
        }
        self.held = Some((t_ms, packet.clone()));
    }

    fn finish(&mut self) {
        if let Some((held_t, held)) = self.held.take() {
            if !self.give_up() && self.speaks(held_t, None) {
                self.decode(held_t, &held);
            }
        }
    }

    /// Decodes `packet` on its own: send, drain to the end, reset. Each frame
    /// therefore belongs to `t_ms`, whatever delay the codec has.
    fn decode(&mut self, t_ms: u64, packet: &ffmpeg::Packet) {
        let mut share = None;
        if self.decoder.send_packet(packet).is_ok() && self.decoder.send_eof().is_ok() {
            let mut decoded = frame::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                if share.is_none() {
                    share = self.luma(&decoded).map(|plane| bottom_text_share(&plane));
                }
            }
        }
        self.decoder.flush();
        match share {
            Some(s) => {
                self.shares.insert(t_ms, s);
            }
            None => self.bad += 1,
        }
    }

    /// `f` scaled to [`TEXT_SAMPLE_WIDTH`] wide, 8-bit luma.
    fn luma(&mut self, f: &frame::Video) -> Option<LumaPlane> {
        let (fmt, w, h) = (f.format(), f.width(), f.height());
        if w == 0 || h == 0 {
            return None;
        }
        let out_w = TEXT_SAMPLE_WIDTH.min(w);
        let out_h = ((u64::from(h) * u64::from(out_w) / u64::from(w)) as u32).max(1);
        let stale =
            !matches!(&self.scaler, Some((sf, sw, sh, _)) if *sf == fmt && *sw == w && *sh == h);
        if stale {
            let ctx = software::scaling::Context::get(
                fmt,
                w,
                h,
                format::Pixel::GRAY8,
                out_w,
                out_h,
                software::scaling::Flags::FAST_BILINEAR,
            )
            .ok()?;
            self.scaler = Some((fmt, w, h, ctx));
        }
        let (_, _, _, sws) = self.scaler.as_mut()?;
        let mut out = frame::Video::empty();
        sws.run(f, &mut out).ok()?;
        Some(LumaPlane {
            width: out.width(),
            height: out.height(),
            stride: out.stride(0),
            data: out.data(0).to_vec(),
        })
    }
}

/// Each window's bottom-text share from the keyframe that speaks for it: the
/// latest keyframe at or before its middle, at most
/// [`TEXT_SAMPLE_MAX_AGE_MS`] before it. `None` when there is no such
/// keyframe or it was not decoded.
pub fn bottom_text_per_line(
    windows: &[(u64, u64)],
    keyframes_ms: &[u64],
    shares: &BTreeMap<u64, f32>,
) -> Vec<Option<f32>> {
    windows
        .iter()
        .map(|&(s, e)| {
            let mid = s / 2 + e / 2;
            let i = keyframes_ms.partition_point(|&k| k <= mid);
            let k = *keyframes_ms.get(i.checked_sub(1)?)?;
            if mid - k > TEXT_SAMPLE_MAX_AGE_MS {
                return None;
            }
            shares.get(&k).copied()
        })
        .collect()
}

/// Loudness, pitch, high-band share and keyframes of `src` for `windows`
/// (`[start_ms, end_ms]` per subtitle line).
///
/// `audio_stream` is a container stream index; `None` picks the stream flagged
/// default, else the first audio stream. A file without audio still reports
/// its keyframes, with every line [`LineSignals::NONE`].
pub fn subtitle_signals(
    src: &Path,
    audio_stream: Option<u32>,
    windows: &[(u64, u64)],
) -> Result<SubtitleSignals, FrameError> {
    crate::libav::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    // The one open of this op.
    let mut ictx = format::input(src).map_err(|e| match e {
        ffmpeg::Error::InvalidData
        | ffmpeg::Error::DemuxerNotFound
        | ffmpeg::Error::StreamNotFound => FrameError::BadInput(format!("open: {e}")),
        other => FrameError::Other(format!("open: {other}")),
    })?;

    let audio_index: Option<usize> = match audio_stream {
        Some(i) => {
            let s = ictx
                .stream(i as usize)
                .ok_or_else(|| FrameError::BadInput(format!("no stream {i}")))?;
            if s.parameters().medium() != media::Type::Audio {
                return Err(FrameError::BadInput(format!("stream {i} is not audio")));
            }
            Some(s.index())
        }
        None => {
            let audio = || {
                ictx.streams()
                    .filter(|s| s.parameters().medium() == media::Type::Audio)
            };
            audio()
                .find(|s| {
                    s.disposition()
                        .contains(format::stream::Disposition::DEFAULT)
                })
                .or_else(|| audio().next())
                .map(|s| s.index())
        }
    };
    let video = ictx
        .streams()
        .best(media::Type::Video)
        .map(|s| (s.index(), TimeBase::of(s.time_base())));

    let sample_cap = windows
        .iter()
        .map(|&(s, e)| s.max(e))
        .max()
        // A window under 400 ms is widened to 400 ms around its centre.
        .map(|end| (end + TAIL_MS + 400) * u64::from(ANALYSIS_RATE) / 1000)
        .unwrap_or(0);

    let mut pass = match audio_index {
        Some(idx) => Some(open_audio(&ictx, idx, windows, sample_cap)?),
        None => None,
    };
    if sample_cap == 0 {
        if let Some(p) = pass.as_mut() {
            p.done = true;
        }
    }
    let channel = match pass.as_ref() {
        Some(p) if p.fc => ChannelPick::FrontCentre,
        _ => ChannelPick::Downmix,
    };

    let mut text = video
        .as_ref()
        .and_then(|(vi, _)| TextPass::open(&ictx, *vi, windows));
    let mut keyframes_ms: Vec<u64> = Vec::new();
    let mut read_errors = 0u32;
    let mut consecutive_errors = 0u32;
    for res in ictx.packets() {
        let (s, packet) = match res {
            Ok(v) => {
                consecutive_errors = 0;
                v
            }
            Err(e) => {
                read_errors += 1;
                consecutive_errors += 1;
                if consecutive_errors >= MAX_READ_ERRORS {
                    tracing::warn!(target: "libav", error = %e, read_errors, "subtitle signals: demux keeps failing, stopping");
                    break;
                }
                continue;
            }
        };
        let idx = s.index();
        if let Some((vi, tb)) = &video {
            if idx == *vi {
                if packet.is_key() {
                    if let Some(ts) = packet.pts().or(packet.dts()) {
                        let t = tb.ms(ts).max(0) as u64;
                        keyframes_ms.push(t);
                        if let Some(tp) = text.as_mut() {
                            tp.key_packet(t, &packet);
                        }
                    }
                }
                continue;
            }
        }
        if let Some(p) = pass.as_mut() {
            if idx == p.stream_index && !p.done {
                p.packet(&packet)?;
            }
        }
    }
    keyframes_ms.sort_unstable();
    keyframes_ms.dedup();
    let cuts_ms = cuts_from_keyframes(&keyframes_ms);
    let bottom_text = match text.as_mut() {
        Some(tp) => {
            tp.finish();
            bottom_text_per_line(windows, &keyframes_ms, &tp.shares)
        }
        None => vec![None; windows.len()],
    };

    let (lines, analysed_seconds) = match pass {
        Some(mut p) => {
            if !p.done {
                let _ = p.decoder.send_eof();
                p.drain()?;
            }
            p.analyser.finish()
        }
        None => (vec![LineSignals::NONE; windows.len()], 0.0),
    };
    Ok(SubtitleSignals {
        lines,
        keyframes_ms,
        cuts_ms,
        bottom_text,
        audio_stream: audio_index.map(|i| i as u32),
        channel,
        analysed_seconds,
        read_errors,
    })
}

/// Decoder, resampler and analyser for the audio stream at `idx`.
fn open_audio(
    ictx: &format::context::Input,
    idx: usize,
    windows: &[(u64, u64)],
    sample_cap: u64,
) -> Result<AudioPass, FrameError> {
    let stream = ictx
        .stream(idx)
        .ok_or_else(|| FrameError::BadInput(format!("no stream {idx}")))?;
    let tb = TimeBase::of(stream.time_base());
    let ctx = codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| FrameError::Other(format!("codec ctx: {e}")))?;
    let decoder = ctx
        .decoder()
        .audio()
        .map_err(|e| FrameError::BadInput(format!("audio decoder: {e}")))?;

    // Native (mask-order) layout for the channel count; see waveform.rs for
    // why a custom layout cannot be handed to `get2`.
    let src_channels = match decoder.ch_layout().channels() {
        0 => 2,
        n => n,
    };
    let native = ChannelLayout::default_for_channels(src_channels);
    // The front-centre channel's position, from the layout the file declares;
    // an unspecified one is taken as the native layout of that many channels.
    // Mono is its own downmix.
    let fc_index = if src_channels >= 3 {
        let declared = decoder.ch_layout();
        let probe = if declared.order() == ChannelOrder::Unspecified {
            &native
        } else {
            &declared
        };
        probe.index_from_channel(Channel::FrontCenter)
    } else {
        None
    };
    let (out_layout, out_format, plane) = match fc_index {
        // Keep every channel (planar, identity layout) and read one plane.
        Some(i) => (
            native.clone(),
            Sample::F32(format::sample::Type::Planar),
            i as usize,
        ),
        None => (
            ChannelLayout::MONO,
            Sample::F32(format::sample::Type::Packed),
            0,
        ),
    };
    let resampler = software::resampling::Context::get2(
        decoder.format(),
        native,
        decoder.rate(),
        out_format,
        out_layout,
        ANALYSIS_RATE,
    )
    .map_err(|e| FrameError::Other(format!("resampler: {e}")))?;
    Ok(AudioPass {
        stream_index: idx,
        tb,
        decoder,
        resampler,
        src_channels,
        plane,
        fc: fc_index.is_some(),
        analyser: SignalAnalyser::new(windows),
        sample_cap,
        done: false,
        bad_frames: 0,
    })
}
