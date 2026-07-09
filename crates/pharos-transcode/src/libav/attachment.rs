//! Extract an embedded attachment stream (a font referenced by ASS/SSA
//! subtitles) to a file. The attachment's bytes live in the stream's codec
//! `extradata` — no decode, just a copy — so this is a cheap, crash-safe op.

use super::frames::FrameError;
use ffmpeg::ffi;
use ffmpeg_the_third as ffmpeg;
use std::path::Path;

/// Write attachment stream `stream_index`'s bytes (the font file) to `out`.
pub fn extract_attachment(src: &Path, stream_index: u32, out: &Path) -> Result<(), FrameError> {
    ffmpeg::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    let ictx =
        ffmpeg::format::input(src).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;
    for stream in ictx.streams() {
        if stream.index() as u32 != stream_index {
            continue;
        }
        // SAFETY: codecpar is valid for the stream's lifetime; extradata is a
        // buffer of `extradata_size` bytes owned by libav.
        let par = unsafe { &*(*stream.as_ptr()).codecpar };
        if par.extradata.is_null() || par.extradata_size <= 0 {
            return Err(FrameError::BadInput(format!(
                "attachment stream {stream_index} carries no data"
            )));
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(par.extradata, par.extradata_size as usize) };
        std::fs::write(out, bytes)
            .map_err(|e| FrameError::Other(format!("write {}: {e}", out.display())))?;
        return Ok(());
    }
    Err(FrameError::BadInput(format!(
        "no stream at index {stream_index}"
    )))
}

/// Dump EVERY embedded attachment (font) to `out_dir/{stream_index}` in a
/// SINGLE source open. ASS/SSA subtitles reference N fonts, and
/// SubtitlesOctopus fetches all of them before rendering a cue; extracting
/// them one at a time re-opens the (often multi-GB, NFS-backed) source N
/// times, which stalls the "Fetching assets" phase. One open, N copies fixes
/// that. Returns the number of attachments written. Best-effort per stream: a
/// stream with no data is skipped rather than failing the batch.
pub fn extract_all_attachments(src: &Path, out_dir: &Path) -> Result<u32, FrameError> {
    ffmpeg::init().map_err(|e| FrameError::Other(format!("libav init: {e}")))?;
    let ictx =
        ffmpeg::format::input(src).map_err(|e| FrameError::BadInput(format!("open: {e}")))?;
    std::fs::create_dir_all(out_dir)
        .map_err(|e| FrameError::Other(format!("mkdir {}: {e}", out_dir.display())))?;
    let mut written = 0u32;
    for stream in ictx.streams() {
        // SAFETY: codecpar is valid for the stream's lifetime; extradata is a
        // buffer of `extradata_size` bytes owned by libav.
        let par = unsafe { &*(*stream.as_ptr()).codecpar };
        if par.codec_type != ffi::AVMediaType::ATTACHMENT {
            continue;
        }
        if par.extradata.is_null() || par.extradata_size <= 0 {
            continue;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(par.extradata, par.extradata_size as usize) };
        let out = out_dir.join(stream.index().to_string());
        std::fs::write(&out, bytes)
            .map_err(|e| FrameError::Other(format!("write {}: {e}", out.display())))?;
        written += 1;
    }
    Ok(written)
}
