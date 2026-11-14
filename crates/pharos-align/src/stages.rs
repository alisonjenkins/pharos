//! The recogniser's stages on real models (spec 042, ADR-0124/0125): the
//! song decoded in stereo, htdemucs's vocals, whisper's words. Each model
//! is loaded for the song and released before the next stage loads.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::audio::{decode_stereo, resample_mono};
use crate::device::{Choice, Device};
use crate::placement::Fallback;
use crate::recognise::whisper::Recogniser;
use crate::separate::htdemucs::Separator;
use crate::separate::{self, vocals_or_mix};
use crate::service::{StageError, Stages};
use crate::transcript::Word;

pub struct Live {
    pub ffmpeg: String,
    /// The device the letter model runs on; the separator and recogniser
    /// follow it.
    pub choice: Choice,
    /// htdemucs's weights, `separate::htdemucs::BPK` in the image.
    pub separator: PathBuf,
    /// whisper's ggml large-v3-turbo.
    pub recogniser: PathBuf,
}

fn failed(fallback: Fallback) -> impl Fn(String) -> StageError {
    move |reason| StageError { fallback, reason }
}

impl Stages for Live {
    fn vocals(&self, path: &Path) -> Result<Vec<f32>, StageError> {
        let mix = decode_stereo(&self.ffmpeg, path, separate::RATE)
            .map_err(|e| failed(Fallback::DecodeFailed)(e.to_string()))?;
        let (vocals, separation) = vocals_or_mix(mix, &self.choice, |adapter, mix| {
            let separator = Separator::load(&self.separator, adapter)?;
            let vocals = separator.vocals(mix);
            separator.release();
            vocals
        });
        let vocals = vocals.ok_or_else(|| failed(Fallback::SeparateFailed)(separation.reason))?;
        resample_mono(&self.ffmpeg, &vocals, separate::RATE)
            .map_err(|e| failed(Fallback::DecodeFailed)(e.to_string()))
    }

    fn recognise(&self, vocals: &[f32]) -> Result<Vec<Word>, StageError> {
        let fail = |e: crate::recognise::whisper::RecogniseError| {
            failed(Fallback::RecogniseFailed)(e.to_string())
        };
        let gpu = matches!(self.choice.device, Device::Vulkan(_));
        let started = Instant::now();
        let recogniser = Recogniser::load(&self.recogniser, gpu).map_err(fail)?;
        let load_ms = started.elapsed().as_millis() as u64;
        let heard = recogniser.transcribe(vocals).map_err(fail)?;
        drop(recogniser);
        tracing::info!(
            gpu,
            load_ms,
            words = heard.transcript.words.len(),
            segments = heard.segments.len(),
            retries = heard.retries.len(),
            "recognised"
        );
        Ok(heard.transcript.words)
    }
}
