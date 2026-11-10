//! One lyric line, end to end after the model: what `Aligner._align_loaded`
//! in `pharos_aligner/model.py` does once it has the emission.

use crate::ctc::{align_words, Emission};
use crate::tokens::{tokenize, STAR_TOKEN};
use crate::AlignError;

/// MMS_FA's input rate.
pub const SAMPLE_RATE: f64 = 16_000.0;

/// Milliseconds of a frame boundary, with model.py's float order and
/// truncation, so results agree at integer boundaries.
pub fn frame_ms(n_samples: usize, frames: usize, frame: usize) -> u64 {
    let ratio = (n_samples as f64 / frames as f64 / SAMPLE_RATE) * 1000.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = (frame as f64 * ratio) as u64;
    ms
}

/// One window's word times and how well its words matched the singing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LineSpans {
    /// `(start_ms, end_ms)` per token within the window.
    pub times: Vec<(u64, u64)>,
    /// The mean `score` (probability) of every character span of the
    /// words, as `mean_score` in model.py; `None` when nothing aligned.
    pub score: Option<f64>,
}

impl From<Vec<(u64, u64)>> for LineSpans {
    fn from(times: Vec<(u64, u64)>) -> Self {
        Self { times, score: None }
    }
}

/// Word times per token within the window, from the window's emission.
///
/// A star after the last word soaks up the music that follows the line;
/// `lead_star` adds one before the first word too, for a window that opens
/// well ahead of the singing. An empty result means nothing to align (no
/// tokens, no frames, or fewer frames than characters), as in model.py.
pub fn align_line<S: AsRef<str>>(
    emission: &Emission<'_>,
    n_samples: usize,
    tokens: &[S],
    lead_star: bool,
) -> Result<LineSpans, AlignError> {
    if n_samples == 0 || tokens.is_empty() || emission.frames() == 0 {
        return Ok(LineSpans::default());
    }
    let lead = usize::from(lead_star);
    let mut all: Vec<&str> = Vec::with_capacity(tokens.len() + 2);
    if lead_star {
        all.push(STAR_TOKEN);
    }
    all.extend(tokens.iter().map(AsRef::as_ref));
    all.push(STAR_TOKEN);
    let tokenized = tokenize(&all)?;
    // CTC needs a frame a character; repeats are left to the aligner's own check.
    if tokenized.iter().map(Vec::len).sum::<usize>() > emission.frames() {
        return Ok(LineSpans::default());
    }
    let spans = align_words(emission, &tokenized)?;
    let frames = emission.frames();
    let words = &spans[lead..spans.len() - 1];
    // Python sums its floats as f64, so the sum is f64 here too.
    let scores: Vec<f64> = words.iter().flatten().map(|c| f64::from(c.score)).collect();
    let score = (!scores.is_empty()).then(|| scores.iter().sum::<f64>() / scores.len() as f64);
    let times = words
        .iter()
        .map(|word| match (word.first(), word.last()) {
            (Some(first), Some(last)) => (
                frame_ms(n_samples, frames, first.start),
                frame_ms(n_samples, frames, last.end),
            ),
            _ => (0, 0),
        })
        .collect();
    Ok(LineSpans { times, score })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::tokens::CLASSES;

    #[test]
    fn twenty_ms_frames() {
        assert_eq!(frame_ms(320 * 100, 100, 50), 1000);
    }

    #[test]
    fn nothing_to_align_is_empty_not_an_error() {
        let e = vec![0.0; 3 * CLASSES];
        let em = Emission::new(&e, 3, CLASSES).unwrap();
        assert_eq!(
            align_line::<&str>(&em, 960, &[], false).unwrap(),
            LineSpans::default()
        );
        assert_eq!(
            align_line(&em, 960, &["abc"], false).unwrap(),
            LineSpans::default(),
            "a window too short for its text has no times and no score"
        );
    }

    /// The score is the mean over the words' character spans only: the
    /// stars around them are left out, as model.py slices them off first.
    #[test]
    fn the_score_is_the_mean_of_the_words_character_spans() {
        let frames = 12;
        // Uneven log-probs, so each span's score differs.
        let e: Vec<f32> = (0..frames * CLASSES)
            .map(|i| -((i * 7919 % 97) as f32) / 20.0)
            .collect();
        let em = Emission::new(&e, frames, CLASSES).unwrap();
        for lead_star in [false, true] {
            let got = align_line(&em, 320 * frames, &["ab", "c"], lead_star).unwrap();
            let mut all = Vec::new();
            if lead_star {
                all.push(STAR_TOKEN);
            }
            all.extend(["ab", "c", STAR_TOKEN]);
            let spans = align_words(&em, &tokenize(&all).unwrap()).unwrap();
            let words = &spans[usize::from(lead_star)..spans.len() - 1];
            let chars: Vec<f64> = words.iter().flatten().map(|c| f64::from(c.score)).collect();
            assert_eq!(chars.len(), 3, "a, b and c");
            let want = chars.iter().sum::<f64>() / 3.0;
            assert_eq!(got.score, Some(want), "lead_star={lead_star}");
            assert!(want > 0.0 && want <= 1.0);
            assert_eq!(got.times.len(), 2);
        }
    }
}
