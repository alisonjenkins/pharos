//! In-process tempo and energy of one song (spec 024, ADR-0049).
//!
//! On the persistent libav worker (V6: the input is a user's file, and a
//! decoder fault must kill a worker, not the server):
//!
//! 1. Open the container and read its BPM tag. A usable tag means the tempo
//!    estimator is skipped; energy still needs the audio.
//! 2. Seek to the window `crate::audio_features::window_for` chose, decode
//!    only that window of the best audio stream, and resample it to mono
//!    `f32` at [`ANALYSIS_RATE`] as it is decoded.
//! 3. Hand the samples to the pure [`crate::audio_features::analyse`].
//!
//! The decode is the whole cost, so it is bounded twice: by the window and by
//! downmixing and downsampling before anything is buffered (30 s of 11 kHz
//! mono is 1.3 MB).

use super::frames::FrameError;
use crate::audio_features::{analyse, parse_bpm_tag, AudioFeatures, ANALYSIS_RATE};
use ffmpeg::ffi;
use ffmpeg::format::Sample;
use ffmpeg::{codec, format, frame, media, software, ChannelLayout};
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Tag keys that carry a tempo. libav's dictionary lookup ignores case, so
/// `BPM` also finds Vorbis/FLAC `bpm` and APE `Bpm`; `TBPM` is ID3v2, `tmpo`
/// the MP4 atom.
const BPM_KEYS: [&str; 3] = ["TBPM", "BPM", "tmpo"];

/// Tempo and energy of `[start_ms, start_ms + dur_ms)` of `src`.
pub fn audio_features(src: &Path, start_ms: u64, dur_ms: u64) -> Result<AudioFeatures, FrameError> {
    crate::libav::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    if dur_ms == 0 {
        return Err(FrameError::Other("dur_ms = 0".into()));
    }
    // Only a container libav cannot make sense of is the file's fault. An I/O
    // error (EIO/ESTALE on an NFS blip, EACCES, ENOENT mid-rename) says
    // nothing about the file, so it stays `Other` and the sweep retries it.
    let mut ictx = format::input(src).map_err(|e| match e {
        ffmpeg::Error::InvalidData
        | ffmpeg::Error::DemuxerNotFound
        | ffmpeg::Error::StreamNotFound => FrameError::BadInput(format!("open: {e}")),
        other => FrameError::Other(format!("open: {other}")),
    })?;

    // The tag first: read before any decode, from the container and then the
    // audio stream (Ogg keeps Vorbis comments on the stream).
    let mut tag_bpm = BPM_KEYS
        .iter()
        .find_map(|k| ictx.metadata().get(k).and_then(parse_bpm_tag));

    let (stream_index, tb_num, tb_den, stream_start, mut decoder) = {
        let stream = ictx
            .streams()
            .best(media::Type::Audio)
            .ok_or_else(|| FrameError::BadInput("no audio stream".into()))?;
        if tag_bpm.is_none() {
            tag_bpm = BPM_KEYS
                .iter()
                .find_map(|k| stream.metadata().get(k).and_then(parse_bpm_tag));
        }
        let tb = stream.time_base();
        let idx = stream.index();
        let (num, den) = (tb.numerator() as i64, tb.denominator() as i64);
        let ctx = codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| FrameError::Other(format!("codec ctx: {e}")))?;
        let decoder = ctx
            .decoder()
            .audio()
            .map_err(|e| FrameError::BadInput(format!("audio decoder: {e}")))?;
        // Timestamps count from the stream's own origin, which is not 0 in
        // every container (an MP3 with a leading ID3/encoder delay, a TS rip).
        // Unknown (`AV_NOPTS_VALUE`) reads as 0.
        let origin = match stream.start_time() {
            ffi::AV_NOPTS_VALUE => 0,
            t => t,
        };
        (idx, num, den, origin, decoder)
    };

    // Native (mask-order) layout for the channel count; see waveform.rs for
    // why a custom layout cannot be handed to `get2`.
    let src_channels = match decoder.ch_layout().channels() {
        0 => 2,
        n => n,
    };
    let src_layout = ChannelLayout::default_for_channels(src_channels);
    let mut resampler = software::resampling::Context::get2(
        decoder.format(),
        src_layout,
        decoder.rate(),
        Sample::F32(format::sample::Type::Packed),
        ChannelLayout::MONO,
        ANALYSIS_RATE,
    )
    .map_err(|e| FrameError::Other(format!("resampler: {e}")))?;

    let tb_ms = |ts: i64| -> i64 {
        if tb_den == 0 {
            return 0;
        }
        (ts - stream_start) * 1000 * tb_num / tb_den
    };
    // The seek target is in AV_TIME_BASE (µs) on the same timeline, so it is
    // offset by the stream's origin too.
    let origin_us = if tb_den == 0 {
        0
    } else {
        stream_start * 1_000_000 * tb_num / tb_den
    };
    // Land at or before the window; the pts gate trims the lead-in. A
    // container that cannot seek is read from the top instead, so the gate
    // never waits for a start it will not reach.
    let start_ms = if start_ms > 0 {
        let start_us = origin_us + (start_ms as i64) * 1000;
        if ictx.seek(start_us, ..start_us).is_ok() {
            start_ms
        } else {
            0
        }
    } else {
        0
    };
    let end_ms = start_ms + dur_ms;

    // The sample count bounds the decode whatever the timestamps say.
    let cap = (dur_ms as usize / 1000 + 1) * ANALYSIS_RATE as usize;
    let mut pcm: Vec<f32> = Vec::with_capacity(cap);
    // A damaged frame is skipped, as ffmpeg itself does; only a window that is
    // nothing but damage gives up early.
    let mut bad_frames = 0u32;
    const MAX_BAD_FRAMES: u32 = 200;
    let mut decoded = frame::Audio::empty();
    // Drain every decodable frame; `true` once the window end (or EOF) is
    // reached, so no further packets are read.
    let mut drain = |decoder: &mut ffmpeg::decoder::Audio,
                     pcm: &mut Vec<f32>,
                     bad_frames: &mut u32|
     -> Result<bool, FrameError> {
        loop {
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    if pcm.len() >= cap {
                        return Ok(true);
                    }
                    // A frame without a timestamp is taken as it comes; the
                    // sample cap ends the window.
                    if let Some(pts_ms) = decoded.pts().map(tb_ms) {
                        if pts_ms >= end_ms as i64 {
                            return Ok(true);
                        }
                        if pts_ms + 100 < start_ms as i64 {
                            continue; // pre-window straggler after the seek
                        }
                    }
                    // SAFETY: `decoded` owns a valid AVFrame; force the native
                    // layout order the resampler was built with.
                    unsafe {
                        let p = decoded.as_mut_ptr();
                        ffi::av_channel_layout_uninit(&mut (*p).ch_layout);
                        ffi::av_channel_layout_default(&mut (*p).ch_layout, src_channels as i32);
                    }
                    let mut mono = frame::Audio::empty();
                    resampler
                        .run(&decoded, &mut mono)
                        .map_err(|e| FrameError::Other(format!("resample: {e}")))?;
                    let n = mono.samples();
                    if n > 0 {
                        let data: &[f32] = mono.plane(0);
                        pcm.extend_from_slice(&data[..n.min(data.len())]);
                    }
                }
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => return Ok(false),
                Err(ffmpeg::Error::Eof) => return Ok(true),
                Err(_) => {
                    *bad_frames += 1;
                    return Ok(*bad_frames > MAX_BAD_FRAMES);
                }
            }
        }
    };

    let mut done = false;
    for res in ictx.packets() {
        if done {
            break;
        }
        let Ok((s, packet)) = res else { continue };
        if s.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            bad_frames += 1;
            done = bad_frames > MAX_BAD_FRAMES;
            continue;
        }
        done = drain(&mut decoder, &mut pcm, &mut bad_frames)?;
    }
    if !done {
        let _ = decoder.send_eof();
        drain(&mut decoder, &mut pcm, &mut bad_frames)?;
    }

    // From here the result stands even if the window was all damage: the tag
    // read above is still the song's tempo.
    let mut features = analyse(&pcm, tag_bpm.is_some());
    features.tag_bpm = tag_bpm;
    Ok(features)
}
