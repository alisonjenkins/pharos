//! In-process chromaprint fingerprint of an audio window (ADR-0018 #1).
//!
//! Decodes `[start_ms, start_ms+dur_ms)` of the best audio stream to
//! interleaved i16 stereo on the persistent libav worker (V6, crash-isolated),
//! and feeds it to the pure-Rust `rusty-chromaprint` fingerprinter — the same
//! output the intro-skipper plugin gets from `ffmpeg -f chromaprint`, minus the
//! ffmpeg-chromaprint build dependency and the per-episode fork.
//!
//! Returns `Vec<u32>` fingerprint points (one per ~0.248 s); the caller aligns
//! them via `crate::fingerprint::align`.

use super::frames::FrameError;
use ffmpeg::ffi;
use ffmpeg::format::Sample;
use ffmpeg::{codec, format, frame, media, software, ChannelLayout};
use ffmpeg_the_third as ffmpeg;
use rusty_chromaprint::{Configuration, Fingerprinter};
use std::path::Path;

/// Fingerprint `dur_ms` of audio starting at `start_ms` into `src`.
pub fn fingerprint_window(src: &Path, start_ms: u64, dur_ms: u64) -> Result<Vec<u32>, FrameError> {
    ffmpeg::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    if dur_ms == 0 {
        return Err(FrameError::Other("dur_ms = 0".into()));
    }
    let mut ictx = format::input(src).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;
    // Build the (owned) decoder inside a block so the stream's immutable
    // borrow of `ictx` ends before the mutable `ictx.seek` below.
    let (stream_index, tb_num, tb_den, mut decoder) = {
        let stream = ictx
            .streams()
            .best(media::Type::Audio)
            .ok_or_else(|| FrameError::BadInput("no audio stream".into()))?;
        let tb = stream.time_base();
        let idx = stream.index();
        let (num, den) = (tb.numerator() as i64, tb.denominator() as i64);
        let ctx = codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| FrameError::Other(format!("codec ctx: {e}")))?;
        let decoder = ctx
            .decoder()
            .audio()
            .map_err(|e| FrameError::BadInput(format!("audio decoder: {e}")))?;
        (idx, num, den, decoder)
    };

    let src_channels = match decoder.ch_layout().channels() {
        0 => 2,
        n => n,
    };
    let src_layout = ChannelLayout::default_for_channels(src_channels);
    let rate = decoder.rate();
    // Interleaved (packed) i16 stereo — what `Fingerprinter::consume` wants,
    // and equivalent to the plugin's `-ac 2`.
    let dst_format = Sample::I16(format::sample::Type::Packed);
    let mut resampler = software::resampling::Context::get2(
        decoder.format(),
        src_layout,
        rate,
        dst_format,
        ChannelLayout::STEREO,
        rate,
    )
    .map_err(|e| FrameError::Other(format!("resampler: {e}")))?;

    // Seek to the window start so a credits (tail) fingerprint doesn't decode
    // the whole file. AV seek is in AV_TIME_BASE (µs); land at-or-before and
    // let the pts gate below trim the lead-in.
    let start_us = (start_ms as i64) * 1000;
    if start_ms > 0 {
        let _ = ictx.seek(start_us, ..start_us);
        // Codec buffers may hold pre-seek frames.
        // (ffmpeg-the-third has no public flush_buffers; a fresh decoder isn't
        // worth it — the pts gate discards any stragglers.)
    }
    let end_ms = start_ms + dur_ms;

    let mut printer = Fingerprinter::new(&Configuration::preset_test2());
    printer
        .start(rate, 2)
        .map_err(|e| FrameError::Other(format!("fingerprinter start: {e}")))?;

    let tb_ms = |ts: i64| -> i64 {
        if tb_den == 0 {
            return 0;
        }
        ts * 1000 * tb_num / tb_den
    };

    let mut decoded = frame::Audio::empty();
    // Drain all currently-decodable frames; returns `true` once the window end
    // (or EOF) is reached so the caller stops feeding packets.
    let mut drain = |decoder: &mut ffmpeg::decoder::Audio,
                     printer: &mut Fingerprinter|
     -> Result<bool, FrameError> {
        loop {
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    let pts_ms = decoded.pts().map(tb_ms).unwrap_or(0);
                    if pts_ms >= end_ms as i64 {
                        return Ok(true);
                    }
                    if pts_ms + 100 < start_ms as i64 {
                        continue; // pre-window straggler after the seek
                    }
                    // SAFETY: force native layout order so swr accepts it.
                    unsafe {
                        let p = decoded.as_mut_ptr();
                        ffi::av_channel_layout_uninit(&mut (*p).ch_layout);
                        ffi::av_channel_layout_default(&mut (*p).ch_layout, src_channels as i32);
                    }
                    let mut out = frame::Audio::empty();
                    resampler
                        .run(&decoded, &mut out)
                        .map_err(|e| FrameError::Other(format!("resample: {e}")))?;
                    let n = out.samples() * 2; // interleaved stereo i16
                    let data: &[i16] = out.plane(0);
                    printer.consume(&data[..n.min(data.len())]);
                }
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => return Ok(false),
                Err(ffmpeg::Error::Eof) => return Ok(true),
                Err(e) => return Err(FrameError::Other(format!("decode: {e}"))),
            }
        }
    };

    let mut done = false;
    for res in ictx.packets() {
        if done {
            break;
        }
        let (s, packet) = match res {
            Ok(sp) => sp,
            Err(_) => continue,
        };
        if s.index() != stream_index {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|e| FrameError::Other(format!("send packet: {e}")))?;
        done = drain(&mut decoder, &mut printer)?;
    }
    if !done {
        let _ = decoder.send_eof();
        drain(&mut decoder, &mut printer)?;
    }

    printer.finish();
    Ok(printer.fingerprint().to_vec())
}
