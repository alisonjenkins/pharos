//! Extract an embedded attachment stream (a font referenced by ASS/SSA
//! subtitles) to a file. The attachment's bytes live in the stream's codec
//! `extradata` — no decode, just a copy — so this is a cheap, crash-safe op.

use super::frames::FrameError;
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
