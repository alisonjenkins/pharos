//! In-process libav transcode for the FFI worker (`backend-lib`).
//!
//! STATUS: WIP scaffold — NOT yet wired into the build (`worker/mod.rs`
//! does not `mod ffi`). The structure (demux → per-stream copy/transcode
//! → mux, both sinks, blocking) is complete, but it needs these
//! ffmpeg-the-third 3.0.2 API fixes to compile:
//!   - audio channel layout: `decoder.channel_layout()` /
//!     `encoder.set_channel_layout()` are the old API; 7.x uses the
//!     `ch_layout` (AVChannelLayout) accessors.
//!   - `ostream.set_parameters(&encoder)` no longer satisfies the
//!     `AsPtr<AVCodecParameters>` bound — copy via
//!     `avcodec_parameters_from_context` or the current params accessor.
//!   - `ParametersRef::as_mut_ptr` for the codec_tag reset moved.
//!   - `EncoderKind` needs a `send_eof`; frame pts must read into a local
//!     before the `set_pts` mutable borrow.
//! Until then the `backend-lib` worker uses the stub in
//! `bin/transcode_worker.rs`; the spawn worker delivers full GPU + CPU
//! balancing in the meantime.
//!
//! v1 scope: a correct **software** transcode (decode → encode → mux) of
//! the video + audio streams to the requested container/codecs, with no
//! fork/exec per operation. This proves the in-process pipeline and the
//! crate's `backend-lib` build; VAAPI-in-libav (hardware frame contexts)
//! is a follow-up — the spawn worker already covers GPU encoding, so the
//! FFI path's value here is eliminating the per-segment process spawn.
//!
//! Runs synchronously (libav is blocking); the worker calls it on a
//! `spawn_blocking` thread. A panic in libav is contained by the worker
//! *process* boundary (V6) — never the server.

use crate::options::{AudioCodec, Container, TranscodeOptions, VideoCodec};
use crate::protocol::{JobSpec, OutputSink, WorkerError};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::{codec, encoder, format, media, Rational};

/// Transcode `spec` entirely in-process. Returns bytes written for the
/// FileDirect sink (0 for the Stdout/pipe sink, where the reader counts).
pub fn transcode(spec: &JobSpec) -> Result<u64, WorkerError> {
    crate::libav::init().map_err(|e| WorkerError::Other(format!("libav init: {e}")))?;

    let input_path = spec
        .input
        .to_str()
        .ok_or_else(|| WorkerError::BadInput(format!("non-utf8 input path: {:?}", spec.input)))?
        .to_string();
    let (out_url, is_file, out_path) = match &spec.sink {
        OutputSink::FileDirect { path } => (
            path.to_str()
                .ok_or_else(|| WorkerError::BadInput(format!("non-utf8 output path: {path:?}")))?
                .to_string(),
            true,
            Some(path.clone()),
        ),
        // libav's `pipe:` protocol writes to the given fd; fd 1 = our
        // stdout, wired to the parent's read pipe.
        OutputSink::Stdout => ("pipe:1".to_string(), false, None),
    };

    let mut ictx = format::input(&input_path).map_err(map_open_err)?;
    // Output muxer: forced by container (the URL may be a bare "pipe:1").
    let muxer = spec.opts.container.ffmpeg_muxer();
    let mut octx = format::output_as(&out_url, muxer)
        .map_err(|e| WorkerError::Other(format!("open output: {e}")))?;

    // Per-input-stream transcode plan, indexed by input stream index.
    let mut plans: Vec<Option<StreamPlan>> = Vec::new();
    let mut video_done = false;
    let mut audio_done = false;

    for istream in ictx.streams() {
        let idx = istream.index();
        while plans.len() <= idx {
            plans.push(None);
        }
        let medium = istream.parameters().medium();
        let plan = match medium {
            media::Type::Video if !video_done => {
                video_done = true;
                Some(build_stream_plan(
                    &istream,
                    &mut octx,
                    StreamKind::Video,
                    &spec.opts,
                )?)
            }
            media::Type::Audio if !audio_done => {
                audio_done = true;
                Some(build_stream_plan(
                    &istream,
                    &mut octx,
                    StreamKind::Audio,
                    &spec.opts,
                )?)
            }
            _ => None,
        };
        plans[idx] = plan;
    }

    octx.write_header()
        .map_err(|e| WorkerError::Other(format!("write header: {e}")))?;

    // Demux → route each packet to its stream plan (copy or decode/encode).
    for (istream, mut packet) in ictx.packets().filter_map(|r| r.ok()) {
        let idx = istream.index();
        let Some(plan) = plans.get_mut(idx).and_then(|p| p.as_mut()) else {
            continue;
        };
        let in_tb = istream.time_base();
        match plan {
            StreamPlan::Copy { out_index, out_tb } => {
                packet.rescale_ts(in_tb, *out_tb);
                packet.set_position(-1);
                packet.set_stream(*out_index);
                packet
                    .write_interleaved(&mut octx)
                    .map_err(|e| WorkerError::Other(format!("write copy pkt: {e}")))?;
            }
            StreamPlan::Transcode(tc) => {
                tc.decoder
                    .send_packet(&packet)
                    .map_err(|e| WorkerError::Other(format!("decode send: {e}")))?;
                drain_decoder(tc, &mut octx, in_tb)?;
            }
        }
    }

    // Flush decoders + encoders.
    for plan in plans.iter_mut().flatten() {
        if let StreamPlan::Transcode(tc) = plan {
            tc.decoder
                .send_eof()
                .map_err(|e| WorkerError::Other(format!("decode eof: {e}")))?;
            drain_decoder(tc, &mut octx, Rational(1, 1))?;
            tc.encoder
                .send_eof()
                .map_err(|e| WorkerError::Other(format!("encode eof: {e}")))?;
            drain_encoder(tc, &mut octx)?;
        }
    }

    octx.write_trailer()
        .map_err(|e| WorkerError::Other(format!("write trailer: {e}")))?;

    let bytes = if is_file {
        out_path
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    };
    Ok(bytes)
}

enum StreamKind {
    Video,
    Audio,
}

enum StreamPlan {
    Copy {
        out_index: usize,
        out_tb: Rational,
    },
    Transcode(Box<Transcoder>),
}

struct Transcoder {
    decoder: DecoderKind,
    encoder: EncoderKind,
    out_index: usize,
    out_tb: Rational,
}

enum DecoderKind {
    Video(ffmpeg::decoder::Video),
    Audio(ffmpeg::decoder::Audio),
}

enum EncoderKind {
    Video(encoder::Video),
    Audio(encoder::Audio),
}

impl DecoderKind {
    fn send_packet(&mut self, p: &ffmpeg::Packet) -> Result<(), ffmpeg::Error> {
        match self {
            DecoderKind::Video(d) => d.send_packet(p),
            DecoderKind::Audio(d) => d.send_packet(p),
        }
    }
    fn send_eof(&mut self) -> Result<(), ffmpeg::Error> {
        match self {
            DecoderKind::Video(d) => d.send_eof(),
            DecoderKind::Audio(d) => d.send_eof(),
        }
    }
}

// (Implementation note: drain_decoder/drain_encoder + build_stream_plan
// live below; kept in one module so the libav lifetimes stay local.)

fn build_stream_plan(
    istream: &ffmpeg::format::stream::Stream,
    octx: &mut format::context::Output,
    kind: StreamKind,
    opts: &TranscodeOptions,
) -> Result<StreamPlan, WorkerError> {
    let copy_requested = match kind {
        StreamKind::Video => matches!(opts.video, Some(VideoCodec::Copy) | None),
        StreamKind::Audio => matches!(opts.audio, Some(AudioCodec::Copy) | None),
    };
    // `None` for video means "no video" — but at the container level we
    // still copy here; the negotiator decides codecs upstream. Treat
    // None as copy to keep the stream rather than dropping it.
    if copy_requested {
        let mut ostream = octx
            .add_stream(encoder::find(codec::Id::None))
            .map_err(|e| WorkerError::Other(format!("add copy stream: {e}")))?;
        ostream.set_parameters(istream.parameters());
        // Don't carry the input codec_tag across containers.
        unsafe {
            (*ostream.parameters().as_mut_ptr()).codec_tag = 0;
        }
        return Ok(StreamPlan::Copy {
            out_index: ostream.index(),
            out_tb: ostream.time_base(),
        });
    }

    let dec_ctx = codec::context::Context::from_parameters(istream.parameters())
        .map_err(|e| WorkerError::Other(format!("decoder ctx: {e}")))?;

    match kind {
        StreamKind::Video => {
            let decoder = dec_ctx
                .decoder()
                .video()
                .map_err(|e| WorkerError::Other(format!("video decoder: {e}")))?;
            let codec_id = match opts.video.unwrap_or(VideoCodec::H264) {
                VideoCodec::H265 => codec::Id::HEVC,
                _ => codec::Id::H264,
            };
            let enc_codec = encoder::find(codec_id).ok_or_else(|| {
                WorkerError::UnsupportedCodec(format!("no {codec_id:?} video encoder in this build"))
            })?;
            let mut enc = codec::context::Context::new_with_codec(enc_codec)
                .encoder()
                .video()
                .map_err(|e| WorkerError::Other(format!("video encoder: {e}")))?;
            enc.set_width(decoder.width());
            enc.set_height(decoder.height());
            enc.set_format(decoder.format());
            let fr = istream.avg_frame_rate();
            enc.set_frame_rate(Some(fr));
            enc.set_time_base(fr.invert());
            if let Some(b) = opts.video_bitrate_bps {
                enc.set_bit_rate(b as usize);
            }
            let mut ostream = octx
                .add_stream(enc_codec)
                .map_err(|e| WorkerError::Other(format!("add video stream: {e}")))?;
            let opened = enc
                .open_as(enc_codec)
                .map_err(|e| WorkerError::Other(format!("open video encoder: {e}")))?;
            ostream.set_parameters(&opened);
            let out_tb = ostream.time_base();
            Ok(StreamPlan::Transcode(Box::new(Transcoder {
                decoder: DecoderKind::Video(decoder),
                encoder: EncoderKind::Video(opened),
                out_index: ostream.index(),
                out_tb,
            })))
        }
        StreamKind::Audio => {
            let decoder = dec_ctx
                .decoder()
                .audio()
                .map_err(|e| WorkerError::Other(format!("audio decoder: {e}")))?;
            let codec_id = match opts.audio.unwrap_or(AudioCodec::Aac) {
                AudioCodec::Mp3 => codec::Id::MP3,
                AudioCodec::Opus => codec::Id::OPUS,
                AudioCodec::Flac => codec::Id::FLAC,
                AudioCodec::Vorbis => codec::Id::VORBIS,
                _ => codec::Id::AAC,
            };
            let enc_codec = encoder::find(codec_id).ok_or_else(|| {
                WorkerError::UnsupportedCodec(format!("no {codec_id:?} audio encoder in this build"))
            })?;
            let mut enc = codec::context::Context::new_with_codec(enc_codec)
                .encoder()
                .audio()
                .map_err(|e| WorkerError::Other(format!("audio encoder: {e}")))?;
            enc.set_rate(decoder.rate() as i32);
            enc.set_channel_layout(decoder.channel_layout());
            enc.set_format(decoder.format());
            enc.set_time_base(Rational(1, decoder.rate() as i32));
            if let Some(b) = opts.audio_bitrate_bps {
                enc.set_bit_rate(b as usize);
            }
            let mut ostream = octx
                .add_stream(enc_codec)
                .map_err(|e| WorkerError::Other(format!("add audio stream: {e}")))?;
            let opened = enc
                .open_as(enc_codec)
                .map_err(|e| WorkerError::Other(format!("open audio encoder: {e}")))?;
            ostream.set_parameters(&opened);
            let out_tb = ostream.time_base();
            Ok(StreamPlan::Transcode(Box::new(Transcoder {
                decoder: DecoderKind::Audio(decoder),
                encoder: EncoderKind::Audio(opened),
                out_index: ostream.index(),
                out_tb,
            })))
        }
    }
}

fn drain_decoder(
    tc: &mut Transcoder,
    octx: &mut format::context::Output,
    _in_tb: Rational,
) -> Result<(), WorkerError> {
    match (&mut tc.decoder, &mut tc.encoder) {
        (DecoderKind::Video(dec), EncoderKind::Video(enc)) => {
            let mut frame = ffmpeg::frame::Video::empty();
            while dec.receive_frame(&mut frame).is_ok() {
                frame.set_pts(frame.timestamp());
                enc.send_frame(&frame)
                    .map_err(|e| WorkerError::Other(format!("video enc send: {e}")))?;
                recv_encoded(enc_packets_v(enc), octx, tc.out_index, tc.out_tb)?;
            }
        }
        (DecoderKind::Audio(dec), EncoderKind::Audio(enc)) => {
            let mut frame = ffmpeg::frame::Audio::empty();
            while dec.receive_frame(&mut frame).is_ok() {
                frame.set_pts(frame.timestamp());
                enc.send_frame(&frame)
                    .map_err(|e| WorkerError::Other(format!("audio enc send: {e}")))?;
                recv_encoded(enc_packets_a(enc), octx, tc.out_index, tc.out_tb)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn drain_encoder(
    tc: &mut Transcoder,
    octx: &mut format::context::Output,
) -> Result<(), WorkerError> {
    match &mut tc.encoder {
        EncoderKind::Video(enc) => recv_encoded(enc_packets_v(enc), octx, tc.out_index, tc.out_tb),
        EncoderKind::Audio(enc) => recv_encoded(enc_packets_a(enc), octx, tc.out_index, tc.out_tb),
    }
}

// Pull encoded packets and interleave-write them, rescaling timestamps to
// the output stream's time base.
fn recv_encoded(
    mut next: impl FnMut(&mut ffmpeg::Packet) -> bool,
    octx: &mut format::context::Output,
    out_index: usize,
    out_tb: Rational,
) -> Result<(), WorkerError> {
    let mut pkt = ffmpeg::Packet::empty();
    while next(&mut pkt) {
        pkt.set_stream(out_index);
        pkt.rescale_ts(out_tb, out_tb);
        pkt.write_interleaved(octx)
            .map_err(|e| WorkerError::Other(format!("write enc pkt: {e}")))?;
    }
    Ok(())
}

fn enc_packets_v<'a>(
    enc: &'a mut encoder::Video,
) -> impl FnMut(&mut ffmpeg::Packet) -> bool + 'a {
    move |pkt| enc.receive_packet(pkt).is_ok()
}

fn enc_packets_a<'a>(
    enc: &'a mut encoder::Audio,
) -> impl FnMut(&mut ffmpeg::Packet) -> bool + 'a {
    move |pkt| enc.receive_packet(pkt).is_ok()
}

fn map_open_err(e: ffmpeg::Error) -> WorkerError {
    // Couldn't open/demux the input → bad input, carrying the libav reason
    // (e.g. "Invalid data found when processing input") so the log is
    // actionable instead of a bare "bad input".
    WorkerError::BadInput(format!("open/demux: {e}"))
}
