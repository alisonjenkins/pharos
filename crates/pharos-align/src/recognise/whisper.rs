//! Whisper large-v3-turbo through whisper.cpp (`whisper-rs`) on Vulkan.

use std::path::Path;

use whisper_rs::{
    DtwMode, DtwModelPreset, DtwParameters, FullParams, SamplingStrategy, WhisperContext,
    WhisperContextParameters, WhisperState,
};

use super::{retry_windows, words, Decode, Retry, Segment, Token};
use crate::audio::RATE;
use crate::transcript::Transcript;

#[derive(Debug, thiserror::Error)]
pub enum RecogniseError {
    #[error("loading whisper model {path}: {message}")]
    Load { path: String, message: String },
    #[error("decoding {start_s:.2}s to {end_s:.2}s: {message}")]
    Decode {
        start_s: f64,
        end_s: f64,
        message: String,
    },
    #[error("token {text:?} at {t0_s:.2}s has no DTW time; DTW is off")]
    NoDtw { text: String, t0_s: f64 },
}

/// What the recogniser heard in one song.
#[derive(Debug, Clone)]
pub struct Heard {
    pub transcript: Transcript,
    pub segments: Vec<Segment>,
    pub retries: Vec<Retry>,
}

pub struct Recogniser {
    ctx: WhisperContext,
    eot: i32,
}

impl Recogniser {
    /// Loads a ggml large-v3-turbo model, on the GPU unless `gpu` is false.
    pub fn load(model: &Path, gpu: bool) -> Result<Self, RecogniseError> {
        let load_err = |message: String| RecogniseError::Load {
            path: model.display().to_string(),
            message,
        };
        let mut cp = WhisperContextParameters::default();
        cp.use_gpu(gpu);
        // whisper.cpp turns DTW off when flash attention is on.
        cp.flash_attn(false);
        cp.dtw_parameters(DtwParameters {
            mode: DtwMode::ModelPreset {
                model_preset: DtwModelPreset::LargeV3Turbo,
            },
            ..DtwParameters::default()
        });
        let path = model
            .to_str()
            .ok_or_else(|| load_err("path is not UTF-8".to_owned()))?;
        let ctx = WhisperContext::new_with_params(path, cp).map_err(|e| load_err(e.to_string()))?;
        let eot = ctx.token_eot();
        Ok(Self { ctx, eot })
    }

    /// Recognises 16 kHz mono audio: the whole song at once, with
    /// whisper.cpp's own seeking (windows cut outside it tripped its DTW
    /// filter on short slices), then each lost window again.
    pub fn transcribe(&self, audio: &[f32]) -> Result<Heard, RecogniseError> {
        let song_s = audio.len() as f64 / f64::from(RATE);
        let first = self.decode(audio, 0.0, song_s, Decode::FIRST)?;
        let (segments, retries) = retry_windows(first, song_s, |start_s, end_s| {
            let at = |s: f64| ((s * f64::from(RATE)) as usize).min(audio.len());
            self.decode(
                &audio[at(start_s)..at(end_s)],
                start_s,
                end_s,
                Decode::RETRY,
            )
        })?;
        for r in &retries {
            r.record();
        }
        let tokens: Vec<Token> = segments.iter().flat_map(|s| s.tokens.clone()).collect();
        Ok(Heard {
            transcript: Transcript {
                words: words(&tokens),
            },
            segments,
            retries,
        })
    }

    /// Decodes `audio`, which starts `start_s` into the song.
    fn decode(
        &self,
        audio: &[f32],
        start_s: f64,
        end_s: f64,
        how: Decode,
    ) -> Result<Vec<Segment>, RecogniseError> {
        let err = |message: String| RecogniseError::Decode {
            start_s,
            end_s,
            message,
        };
        let mut state: WhisperState = self.ctx.create_state().map_err(|e| err(e.to_string()))?;
        state
            .full(params(how), audio)
            .map_err(|e| err(e.to_string()))?;
        let cs = |t: i64| start_s + t as f64 / 100.0;
        let mut out = Vec::new();
        for i in 0..state.full_n_segments() {
            let Some(seg) = state.get_segment(i) else {
                continue;
            };
            let mut tokens = Vec::new();
            for j in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(j) else {
                    continue;
                };
                if tok.token_id() >= self.eot {
                    continue;
                }
                let d = tok.token_data();
                let text = tok
                    .to_str_lossy()
                    .map_err(|e| err(e.to_string()))?
                    .into_owned();
                if d.t_dtw < 0 {
                    return Err(RecogniseError::NoDtw {
                        text,
                        t0_s: cs(d.t0),
                    });
                }
                tokens.push(Token {
                    text,
                    prob: d.p,
                    t0_s: cs(d.t0),
                    dtw_s: cs(d.t_dtw),
                });
            }
            out.push(Segment {
                start_s: cs(seg.start_timestamp()),
                end_s: cs(seg.end_timestamp()),
                text: seg
                    .to_str_lossy()
                    .map_err(|e| err(e.to_string()))?
                    .into_owned(),
                tokens,
            });
        }
        Ok(out)
    }
}

fn params(how: Decode) -> FullParams<'static, 'static> {
    let mut p = match how.beam {
        Some(beam_size) => FullParams::new(SamplingStrategy::BeamSearch {
            beam_size,
            patience: -1.0,
        }),
        None => FullParams::new(SamplingStrategy::Greedy { best_of: 5 }),
    };
    p.set_language(Some("en"));
    p.set_no_context(true);
    p.set_n_max_text_ctx(how.n_max_text_ctx);
    p.set_entropy_thold(how.entropy_thold);
    p.set_token_timestamps(true);
    p.set_print_special(false);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_timestamps(false);
    p
}
