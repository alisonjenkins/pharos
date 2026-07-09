//! Shared in-process video frame pipeline: open a file, optionally seek,
//! run the best video stream through a libav filter spec (e.g.
//! `scale=480:-1` for a thumbnail, `fps=1/10,scale=320:-2,tile=10x10` for
//! a trickplay sprite sheet), and hand each filtered frame to a callback.
//! Plus an MJPEG single-frame encoder so the callback can write JPEGs
//! byte-equivalent to the spawn path's `-f mjpeg` output.
//!
//! Software-only, blocking. Used by `image` and `trickplay`.

use ffmpeg::ffi;
use ffmpeg::{codec, filter, format, frame, media, software, Rational};
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

#[derive(Debug)]
pub enum FrameError {
    /// libav could not open / decode the input — non-recoverable, maps to
    /// the worker's `BadInput`.
    BadInput(String),
    /// Encoder / filter / internal error — maps to the worker's `Other`.
    Other(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::BadInput(s) => write!(f, "bad input: {s}"),
            FrameError::Other(s) => write!(f, "{s}"),
        }
    }
}

/// libav `EAGAIN` (need more input) — distinct from a real error in the
/// send/receive drain loops.
fn is_eagain(e: &ffmpeg::Error) -> bool {
    matches!(e, ffmpeg::Error::Other { errno } if *errno == libc::EAGAIN)
}

/// Pixel format's canonical libav name (e.g. "yuv420p") for the `buffer`
/// source filter args.
fn pix_name(p: format::Pixel) -> String {
    let raw = ffi::AVPixelFormat::from(p);
    // SAFETY: raw is a valid AVPixelFormat; name fn returns a static cstr
    // or null.
    unsafe {
        let n = ffi::av_get_pix_fmt_name(raw);
        if n.is_null() {
            "yuv420p".to_string()
        } else {
            std::ffi::CStr::from_ptr(n).to_string_lossy().into_owned()
        }
    }
}

/// Build the `buffer -> <filter_spec> -> buffersink` graph for a decoded
/// video stream. Shared by the full-stream [`filter_video`] and the
/// seek-sampling [`filter_video_seeked`] drivers.
fn build_video_filter_graph(
    decoder: &ffmpeg::decoder::Video,
    time_base: Rational,
    filter_spec: &str,
    sink_format: format::Pixel,
) -> Result<filter::Graph, FrameError> {
    let mut graph = filter::Graph::new();
    let sar = decoder.aspect_ratio();
    let sar = if sar.numerator() == 0 {
        Rational(1, 1)
    } else {
        sar
    };
    let args = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}",
        decoder.width(),
        decoder.height(),
        pix_name(decoder.format()),
        time_base.numerator(),
        time_base.denominator(),
        sar.numerator(),
        sar.denominator(),
    );
    graph
        .add(
            &filter::find("buffer").ok_or_else(|| FrameError::Other("no buffer filter".into()))?,
            "in",
            &args,
        )
        .map_err(|e| FrameError::Other(format!("buffer: {e}")))?;
    graph
        .add(
            &filter::find("buffersink")
                .ok_or_else(|| FrameError::Other("no buffersink filter".into()))?,
            "out",
            "",
        )
        .map_err(|e| FrameError::Other(format!("buffersink: {e}")))?;
    if let Some(mut out) = graph.get("out") {
        out.set_pixel_format(sink_format);
    }
    graph
        .output("in", 0)
        .and_then(|p| p.input("out", 0))
        .and_then(|p| p.parse(filter_spec))
        .map_err(|e| FrameError::Other(format!("parse filter '{filter_spec}': {e}")))?;
    graph
        .validate()
        .map_err(|e| FrameError::Other(format!("graph validate: {e}")))?;
    Ok(graph)
}

/// Seek-sampling counterpart to [`filter_video`]. Rather than demuxing the
/// whole file, seek to each timestamp in `seek_targets_ms` and decode a
/// single keyframe there, pushing it through `filter_spec` (typically a
/// `scale,tile` graph with NO `fps` — the sampling is done here by seeking).
///
/// This is the trickplay path: a whole-file walk reads every byte of a
/// multi-GB episode over NFS and, under scan + playback contention, blew past
/// the worker's heavy-op timeout (`worker died mid-op: op timeout`) so no
/// sprite was ever produced. Seeking reads only the region around each sample
/// point — an order of magnitude less I/O — so the op finishes well inside the
/// cap. Fed frames get a synthetic monotonic PTS (the sample index) so the
/// downstream `tile` filter groups them deterministically even when adjacent
/// seeks land on the same keyframe.
///
/// Returns the number of filtered output frames delivered (sprite sheets).
pub fn filter_video_seeked<I, F>(
    path: &Path,
    seek_targets_ms: I,
    filter_spec: &str,
    sink_format: format::Pixel,
    mut on_frame: F,
) -> Result<usize, FrameError>
where
    I: IntoIterator<Item = u64>,
    F: FnMut(&frame::Video) -> Result<bool, FrameError>,
{
    ffmpeg::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    let mut ictx = format::input(path).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;

    let stream = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| FrameError::BadInput("no video stream".into()))?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let params = stream.parameters();

    let ctx = codec::context::Context::from_parameters(params)
        .map_err(|e| FrameError::Other(format!("codec ctx: {e}")))?;
    let mut decoder = ctx
        .decoder()
        .video()
        .map_err(|e| FrameError::BadInput(format!("video decoder: {e}")))?;
    // Each sample lands on the nearest keyframe anyway, so decode keyframes
    // only — the seek target's keyframe is the first (and only) frame we need.
    decoder.skip_frame(ffmpeg::Discard::NonKey);

    let mut graph = build_video_filter_graph(&decoder, time_base, filter_spec, sink_format)?;

    let mut delivered = 0usize;
    let mut stop = false;
    let mut decoded = frame::Video::empty();
    let mut sample_idx: i64 = 0;

    // Pull every ready sheet out of the sink.
    let mut drain_sink = |graph: &mut filter::Graph,
                          delivered: &mut usize,
                          stop: &mut bool|
     -> Result<(), FrameError> {
        let mut filtered = frame::Video::empty();
        loop {
            let mut ctx = match graph.get("out") {
                Some(c) => c,
                None => return Ok(()),
            };
            match ctx.sink().frame(&mut filtered) {
                Ok(()) => {
                    *delivered += 1;
                    if !on_frame(&filtered)? {
                        *stop = true;
                        return Ok(());
                    }
                }
                Err(e) if is_eagain(&e) => return Ok(()),
                Err(ffmpeg::Error::Eof) => return Ok(()),
                Err(e) => return Err(FrameError::Other(format!("sink: {e}"))),
            }
        }
    };

    for target_ms in seek_targets_ms {
        // Seek to the keyframe at/<= target (AV_TIME_BASE units, like `-ss`).
        let ts = (target_ms as i128 * ffi::AV_TIME_BASE as i128 / 1000) as i64;
        if ictx.seek(ts, ..=ts).is_err() {
            break; // unseekable / past end — stop cleanly, keep what we have
        }
        decoder.flush(); // drop pre-seek decoder state

        // Decode the first frame at the new position.
        let mut got = false;
        for res in ictx.packets() {
            let (stream, packet) = match res {
                Ok(sp) => sp,
                Err(_) => continue,
            };
            if stream.index() != stream_index {
                continue;
            }
            decoder
                .send_packet(&packet)
                .map_err(|e| FrameError::Other(format!("send packet: {e}")))?;
            match decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    // Synthetic monotonic PTS so `tile` groups deterministically.
                    decoded.set_pts(Some(sample_idx));
                    sample_idx += 1;
                    if let Some(mut src) = graph.get("in") {
                        src.source()
                            .add(&decoded)
                            .map_err(|e| FrameError::Other(format!("src add: {e}")))?;
                    }
                    drain_sink(&mut graph, &mut delivered, &mut stop)?;
                    got = true;
                    break;
                }
                Err(e) if is_eagain(&e) => continue, // need another packet
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(FrameError::Other(format!("decode: {e}"))),
            }
        }
        if stop || !got {
            break; // stop requested, or no frame at this seek (EOF)
        }
    }

    // Flush the graph so the final partial sheet is padded + emitted.
    if !stop {
        if let Some(mut src) = graph.get("in") {
            let _ = src.source().flush();
        }
        drain_sink(&mut graph, &mut delivered, &mut stop)?;
    }

    Ok(delivered)
}

/// Open `path`, seek to `seek_ms` (input seek; `None`/0 = start), and run
/// the best video stream through `filter_spec`. `on_frame` is called for
/// every filtered output frame; return `Ok(false)` from it to stop early
/// (e.g. after the first frame for a thumbnail). Returns the number of
/// frames delivered.
pub fn filter_video<F>(
    path: &Path,
    seek_ms: Option<u64>,
    filter_spec: &str,
    sink_format: format::Pixel,
    keyframe_only: bool,
    mut on_frame: F,
) -> Result<usize, FrameError>
where
    F: FnMut(&frame::Video) -> Result<bool, FrameError>,
{
    ffmpeg::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    let mut ictx = format::input(path).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;

    let stream = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| FrameError::BadInput("no video stream".into()))?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let params = stream.parameters();

    let ctx = codec::context::Context::from_parameters(params)
        .map_err(|e| FrameError::Other(format!("codec ctx: {e}")))?;
    let mut decoder = ctx
        .decoder()
        .video()
        .map_err(|e| FrameError::BadInput(format!("video decoder: {e}")))?;
    // Keyframe-only decode: for interval-sampled work (trickplay) we throw
    // away >99% of decoded frames, so skip non-keyframes in the decoder
    // entirely — an order-of-magnitude less decode work. Preview placement
    // snaps to the nearest keyframe, which is imperceptible in a scrub strip.
    // (Jellyfin's `EnableKeyFrameOnlyExtraction`, but on by default here.)
    if keyframe_only {
        decoder.skip_frame(ffmpeg::Discard::NonKey);
    }

    // Input seek in AV_TIME_BASE (1e6) units, mirroring ffmpeg's `-ss`
    // before `-i`. Seek to <= target so the first decoded frame covers it.
    if let Some(ms) = seek_ms.filter(|m| *m > 0) {
        let ts = (ms as i128 * ffi::AV_TIME_BASE as i128 / 1000) as i64;
        let _ = ictx.seek(ts, ..=ts);
    }

    let mut graph = build_video_filter_graph(&decoder, time_base, filter_spec, sink_format)?;

    let mut delivered = 0usize;
    let mut stop = false;

    // Pull every ready frame out of the sink, applying on_frame.
    let mut drain_sink = |graph: &mut filter::Graph,
                          delivered: &mut usize,
                          stop: &mut bool|
     -> Result<(), FrameError> {
        let mut filtered = frame::Video::empty();
        loop {
            let mut ctx = match graph.get("out") {
                Some(c) => c,
                None => return Ok(()),
            };
            match ctx.sink().frame(&mut filtered) {
                Ok(()) => {
                    *delivered += 1;
                    if !on_frame(&filtered)? {
                        *stop = true;
                        return Ok(());
                    }
                }
                Err(e) if is_eagain(&e) => return Ok(()),
                Err(ffmpeg::Error::Eof) => return Ok(()),
                Err(e) => return Err(FrameError::Other(format!("sink: {e}"))),
            }
        }
    };

    // Feed decoded frames through the graph.
    let mut decoded = frame::Video::empty();
    let mut feed = |graph: &mut filter::Graph,
                    dec: &mut ffmpeg::decoder::Video,
                    delivered: &mut usize,
                    stop: &mut bool,
                    decoded: &mut frame::Video|
     -> Result<(), FrameError> {
        loop {
            match dec.receive_frame(decoded) {
                Ok(()) => {
                    if let Some(mut src) = graph.get("in") {
                        src.source()
                            .add(decoded)
                            .map_err(|e| FrameError::Other(format!("src add: {e}")))?;
                    }
                    drain_sink(graph, delivered, stop)?;
                    if *stop {
                        return Ok(());
                    }
                }
                Err(e) if is_eagain(&e) => return Ok(()),
                Err(ffmpeg::Error::Eof) => return Ok(()),
                Err(e) => return Err(FrameError::Other(format!("decode: {e}"))),
            }
        }
    };

    // Stream packets one at a time — do NOT collect the whole compressed
    // stream first. A previous `.collect()` here loaded every packet of the
    // source into a Vec before decoding, so trickplay/thumbnail generation
    // over a full-length episode buffered the entire ~1-2 GB compressed video
    // in RAM at once (× per width / concurrent op → OOM-killed the pod). The
    // decoder + filter graph are independent of `ictx`, so borrowing it for
    // the packet iterator here is fine.
    for res in ictx.packets() {
        let (stream, packet) = match res {
            Ok(sp) => sp,
            Err(_) => continue,
        };
        if stream.index() != stream_index {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|e| FrameError::Other(format!("send packet: {e}")))?;
        feed(
            &mut graph,
            &mut decoder,
            &mut delivered,
            &mut stop,
            &mut decoded,
        )?;
        if stop {
            break;
        }
    }

    if !stop {
        // Flush decoder, then the filter graph.
        let _ = decoder.send_eof();
        feed(
            &mut graph,
            &mut decoder,
            &mut delivered,
            &mut stop,
            &mut decoded,
        )?;
        if !stop {
            if let Some(mut src) = graph.get("in") {
                let _ = src.source().flush();
            }
            drain_sink(&mut graph, &mut delivered, &mut stop)?;
        }
    }

    Ok(delivered)
}

/// Encode one filtered frame to a standalone JPEG (raw MJPEG packet =
/// valid JFIF). `quality` maps to the encoder's global quality (FFmpeg
/// `-q:v`). Returns the JPEG bytes.
pub fn encode_jpeg(src: &frame::Video, quality: i32) -> Result<Vec<u8>, FrameError> {
    // MJPEG wants a full-range YUVJ pixel format.
    let target = format::Pixel::YUVJ420P;
    let frame = if src.format() == target {
        // Borrow as-is via a shallow clone of the frame view.
        clone_video(src)
    } else {
        let mut sws = software::scaling::Context::get(
            src.format(),
            src.width(),
            src.height(),
            target,
            src.width(),
            src.height(),
            software::scaling::Flags::BILINEAR,
        )
        .map_err(|e| FrameError::Other(format!("sws ctx: {e}")))?;
        let mut out = frame::Video::empty();
        sws.run(src, &mut out)
            .map_err(|e| FrameError::Other(format!("sws run: {e}")))?;
        out
    };

    let enc_codec = ffmpeg::encoder::find(codec::Id::MJPEG)
        .ok_or_else(|| FrameError::Other("no mjpeg encoder".into()))?;
    let ctx = codec::context::Context::new_with_codec(enc_codec);
    let mut enc = ctx
        .encoder()
        .video()
        .map_err(|e| FrameError::Other(format!("mjpeg enc: {e}")))?;
    enc.set_width(frame.width());
    enc.set_height(frame.height());
    enc.set_format(target);
    enc.set_time_base(Rational(1, 25));
    enc.set_global_quality(quality);
    let mut opened = enc
        .open()
        .map_err(|e| FrameError::Other(format!("open mjpeg: {e}")))?;

    opened
        .send_frame(&frame)
        .map_err(|e| FrameError::Other(format!("send frame: {e}")))?;
    opened
        .send_eof()
        .map_err(|e| FrameError::Other(format!("enc eof: {e}")))?;

    let mut out = Vec::new();
    let mut packet = codec::packet::Packet::empty();
    loop {
        match opened.receive_packet(&mut packet) {
            Ok(()) => {
                if let Some(data) = packet.data() {
                    out.extend_from_slice(data);
                }
            }
            Err(e) if is_eagain(&e) => break,
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => return Err(FrameError::Other(format!("recv packet: {e}"))),
        }
    }
    if out.is_empty() {
        return Err(FrameError::Other("mjpeg produced no data".into()));
    }
    Ok(out)
}

/// Deep-copy a video frame (used when the source format already matches
/// the encoder so we still own a frame to hand off).
fn clone_video(src: &frame::Video) -> frame::Video {
    let mut dst = frame::Video::new(src.format(), src.width(), src.height());
    dst.clone_from(src);
    dst
}
