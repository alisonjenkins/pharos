//! In-process audio waveform RMS — replaces the `ffmpeg -af
//! aresample,aformat=mono,asetnsamples=N,astats,ametadata=print` spawn in
//! `backend/spawn.rs`. Decodes the best audio stream, resamples to mono
//! f32, and computes per-bin RMS in dBFS (silent bins → 0.0, matching the
//! spawn path's `-inf` handling).

use super::frames::FrameError;
use ffmpeg::ffi;
use ffmpeg::format::Sample;
use ffmpeg::ChannelLayout;
use ffmpeg::{codec, format, frame, media, software};
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Compute `target_bins` RMS-dBFS readings from `src`, one per
/// `samples_per_bin` mono samples. Always returns exactly `target_bins`
/// values (zero-padded / truncated), matching the spawn contract.
pub fn waveform_rms(
    src: &Path,
    samples_per_bin: u64,
    target_bins: u32,
) -> Result<Vec<f32>, FrameError> {
    crate::libav::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    if samples_per_bin == 0 {
        return Err(FrameError::Other("samples_per_bin = 0".into()));
    }
    let mut ictx = format::input(src).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;
    let stream = ictx
        .streams()
        .best(media::Type::Audio)
        .ok_or_else(|| FrameError::BadInput("no audio stream".into()))?;
    let stream_index = stream.index();
    let params = stream.parameters();

    let ctx = codec::context::Context::from_parameters(params)
        .map_err(|e| FrameError::Other(format!("codec ctx: {e}")))?;
    let mut decoder = ctx
        .decoder()
        .audio()
        .map_err(|e| FrameError::BadInput(format!("audio decoder: {e}")))?;

    // Use a native (mask-order) layout for the channel count: the crate's
    // `get2` stores `layout.mask().unwrap()` internally, which panics on a
    // custom/unspecified layout. `default_for_channels` is always native.
    let src_channels = match decoder.ch_layout().channels() {
        0 => 2,
        n => n,
    };
    let src_layout = ChannelLayout::default_for_channels(src_channels);
    let dst_format = Sample::F32(format::sample::Type::Packed);
    let mut resampler = software::resampling::Context::get2(
        decoder.format(),
        src_layout,
        decoder.rate(),
        dst_format,
        ChannelLayout::MONO,
        decoder.rate(),
    )
    .map_err(|e| FrameError::Other(format!("resampler: {e}")))?;

    let mut bins: Vec<f32> = Vec::with_capacity(target_bins as usize);
    let mut acc_sq: f64 = 0.0;
    let mut acc_n: u64 = 0;

    let push_sample = |s: f32, bins: &mut Vec<f32>, acc_sq: &mut f64, acc_n: &mut u64| {
        *acc_sq += (s as f64) * (s as f64);
        *acc_n += 1;
        if *acc_n >= samples_per_bin {
            let rms = (*acc_sq / *acc_n as f64).sqrt();
            let db = if rms > 0.0 { 20.0 * rms.log10() } else { 0.0 };
            bins.push(db as f32);
            *acc_sq = 0.0;
            *acc_n = 0;
        }
    };

    let drain_decoder = |decoder: &mut ffmpeg::decoder::Audio,
                         resampler: &mut software::resampling::Context,
                         bins: &mut Vec<f32>,
                         acc_sq: &mut f64,
                         acc_n: &mut u64|
     -> Result<(), FrameError> {
        let mut decoded = frame::Audio::empty();
        loop {
            if bins.len() >= target_bins as usize {
                return Ok(());
            }
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    // Force the frame's channel layout to the native
                    // mask order the resampler was built with, so
                    // `swr_convert_frame` doesn't reject it as
                    // "Input changed" on unspecified-order layouts.
                    // SAFETY: `decoded` owns a valid AVFrame.
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
                    let data: &[f32] = mono.plane(0);
                    for &s in &data[..n.min(data.len())] {
                        push_sample(s, bins, acc_sq, acc_n);
                    }
                }
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => return Ok(()),
                Err(ffmpeg::Error::Eof) => return Ok(()),
                Err(e) => return Err(FrameError::Other(format!("decode: {e}"))),
            }
        }
    };

    let packets: Vec<_> = ictx
        .packets()
        .filter_map(|r| r.ok())
        .filter(|(s, _)| s.index() == stream_index)
        .map(|(_, p)| p)
        .collect();
    for packet in packets {
        if bins.len() >= target_bins as usize {
            break;
        }
        decoder
            .send_packet(&packet)
            .map_err(|e| FrameError::Other(format!("send packet: {e}")))?;
        drain_decoder(
            &mut decoder,
            &mut resampler,
            &mut bins,
            &mut acc_sq,
            &mut acc_n,
        )?;
    }
    if bins.len() < target_bins as usize {
        let _ = decoder.send_eof();
        drain_decoder(
            &mut decoder,
            &mut resampler,
            &mut bins,
            &mut acc_sq,
            &mut acc_n,
        )?;
    }

    bins.resize(target_bins as usize, 0.0);
    Ok(bins)
}
