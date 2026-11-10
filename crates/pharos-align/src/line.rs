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

/// `(start_ms, end_ms)` per token within the window, from the window's emission.
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
) -> Result<Vec<(u64, u64)>, AlignError> {
    if n_samples == 0 || tokens.is_empty() || emission.frames() == 0 {
        return Ok(Vec::new());
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
        return Ok(Vec::new());
    }
    let spans = align_words(emission, &tokenized)?;
    let frames = emission.frames();
    Ok(spans[lead..spans.len() - 1]
        .iter()
        .map(|word| match (word.first(), word.last()) {
            (Some(first), Some(last)) => (
                frame_ms(n_samples, frames, first.start),
                frame_ms(n_samples, frames, last.end),
            ),
            _ => (0, 0),
        })
        .collect())
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
        assert_eq!(align_line::<&str>(&em, 960, &[], false).unwrap(), vec![]);
        assert_eq!(align_line(&em, 960, &["abc"], false).unwrap(), vec![]);
    }
}
