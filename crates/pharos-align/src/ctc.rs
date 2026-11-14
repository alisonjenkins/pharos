//! CTC forced alignment: a port of torchaudio's CPU `forced_align`
//! (`src/libtorchaudio/forced_align/cpu/compute.cpp`, v2.11.0) and
//! `merge_tokens`, kept step for step so paths match bit for bit.
//!
//! Alphas stay in f32 like torchaudio's `scalar_t`, and its tie-breaking is
//! kept as written, including taking the stay transition when the skip and
//! advance scores tie above it: near-tied paths would otherwise diverge.

use crate::tokens::BLANK;
use crate::AlignError;

/// Log-probability emissions, row-major `[frames, classes]`.
#[derive(Debug, Clone, Copy)]
pub struct Emission<'a> {
    data: &'a [f32],
    frames: usize,
    classes: usize,
}

impl<'a> Emission<'a> {
    pub fn new(data: &'a [f32], frames: usize, classes: usize) -> Result<Self, AlignError> {
        if frames.checked_mul(classes) != Some(data.len()) {
            return Err(AlignError::EmissionShape {
                len: data.len(),
                frames,
                classes,
            });
        }
        Ok(Emission {
            data,
            frames,
            classes,
        })
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn classes(&self) -> usize {
        self.classes
    }

    /// Frames `lo..hi`, clamped to the emission.
    pub fn frames_between(&self, lo: usize, hi: usize) -> Emission<'a> {
        let hi = hi.min(self.frames);
        let lo = lo.min(hi);
        Emission {
            data: &self.data[lo * self.classes..hi * self.classes],
            frames: hi - lo,
            classes: self.classes,
        }
    }

    pub(crate) fn at(&self, t: usize, c: u32) -> f32 {
        self.data[t * self.classes + c as usize]
    }
}

/// A target token held over a run of frames.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenSpan {
    pub token: u32,
    /// First frame, inclusive.
    pub start: usize,
    /// Last frame, exclusive.
    pub end: usize,
    /// Mean probability (not log) of the token over its frames.
    pub score: f32,
}

/// The best CTC path: one label per frame, and that label's log probability.
#[derive(Debug, Clone, PartialEq)]
pub struct Alignment {
    pub path: Vec<u32>,
    pub log_scores: Vec<f32>,
}

/// Consecutive equal targets: each needs a blank frame between its copies.
fn repeats(targets: &[u32]) -> usize {
    targets.windows(2).filter(|w| w[0] == w[1]).count()
}

/// The Viterbi path of `targets` (no blanks) through `emission`.
pub fn forced_align(emission: &Emission<'_>, targets: &[u32]) -> Result<Alignment, AlignError> {
    if targets.is_empty() {
        return Err(AlignError::NoTargets);
    }
    if let Some(&token) = targets
        .iter()
        .find(|&&t| t == BLANK || t as usize >= emission.classes)
    {
        return Err(AlignError::BadTarget {
            token,
            classes: emission.classes,
        });
    }
    let t_len = emission.frames;
    let l = targets.len();
    let r = repeats(targets);
    if t_len < l + r {
        return Err(AlignError::TooFewFrames {
            frames: t_len,
            targets: l,
            repeats: r,
        });
    }
    let s = 2 * l + 1;
    let label = |i: usize| if i % 2 == 0 { BLANK } else { targets[i / 2] };
    // An index past the targets reads as "differs"; torchaudio's bounds keep
    // it from happening, and this keeps the port from reading out of range.
    let differs = |a: usize, b: usize| targets.get(a) != targets.get(b);

    let mut alphas = vec![f32::NEG_INFINITY; 2 * s];
    let mut back = vec![-1i8; t_len * s];
    let mut start = if t_len > l + r { 0 } else { 1 };
    let mut end = if s == 1 { 1 } else { 2 };
    for (i, a) in alphas.iter_mut().enumerate().take(end).skip(start) {
        *a = emission.at(0, label(i));
    }
    for t in 1..t_len {
        if t_len - t <= l + r {
            if start % 2 == 1 && differs(start / 2, start / 2 + 1) {
                start += 1;
            }
            start += 1;
        }
        if t <= l + r {
            if end % 2 == 0 && end < 2 * l && differs(end / 2 - 1, end / 2) {
                end += 1;
            }
            end += 1;
        }
        let cur = (t % 2) * s;
        let prev = ((t - 1) % 2) * s;
        alphas[cur..cur + s].fill(f32::NEG_INFINITY);
        let mut startloop = start;
        if start == 0 {
            alphas[cur] = alphas[prev] + emission.at(t, BLANK);
            back[t * s] = 0;
            startloop += 1;
        }
        for i in startloop..end {
            let x0 = alphas[prev + i];
            let x1 = alphas[prev + i - 1];
            let x2 = if i % 2 != 0 && i != 1 && differs(i / 2, i / 2 - 1) {
                alphas[prev + i - 2]
            } else {
                f32::NEG_INFINITY
            };
            let (best, ptr) = if x2 > x1 && x2 > x0 {
                (x2, 2)
            } else if x1 > x0 && x1 > x2 {
                (x1, 1)
            } else {
                (x0, 0)
            };
            alphas[cur + i] = best + emission.at(t, label(i));
            back[t * s + i] = ptr;
        }
    }
    let last = ((t_len - 1) % 2) * s;
    let mut state = if alphas[last + s - 1] > alphas[last + s - 2] {
        s - 1
    } else {
        s - 2
    };
    let mut path = vec![BLANK; t_len];
    for t in (0..t_len).rev() {
        path[t] = label(state);
        if t > 0 {
            // torchaudio subtracts the pointer unchecked; a feasible target
            // never reaches an unset (-1) entry, but stay in range regardless.
            let next = state.checked_add_signed(-isize::from(back[t * s + state]));
            state = next
                .filter(|&n| n < s)
                .ok_or(AlignError::BrokenPath { frame: t })?;
        }
    }
    let log_scores = path
        .iter()
        .enumerate()
        .map(|(t, &c)| emission.at(t, c))
        .collect();
    Ok(Alignment { path, log_scores })
}

/// Collapses a frame path into one span per target token, as
/// `torchaudio.functional.merge_tokens(path, log_scores.exp())` does.
pub fn merge_tokens(alignment: &Alignment) -> Vec<TokenSpan> {
    let path = &alignment.path;
    let mut spans = Vec::new();
    let mut start = 0;
    while start < path.len() {
        let token = path[start];
        let end = start + path[start..].iter().take_while(|&&c| c == token).count();
        if token != BLANK {
            let probs = &alignment.log_scores[start..end];
            let sum: f64 = probs.iter().map(|&p| f64::from(p.exp())).sum();
            #[allow(clippy::cast_possible_truncation)]
            let score = (sum / (end - start) as f64) as f32;
            spans.push(TokenSpan {
                token,
                start,
                end,
                score,
            });
        }
        start = end;
    }
    spans
}

/// Aligns words of label sequences and splits the spans back per word, as
/// `bundle.get_aligner()` does.
pub fn align_words(
    emission: &Emission<'_>,
    words: &[Vec<u32>],
) -> Result<Vec<Vec<TokenSpan>>, AlignError> {
    let flat: Vec<u32> = words.iter().flatten().copied().collect();
    let spans = merge_tokens(&forced_align(emission, &flat)?);
    if spans.len() != flat.len() {
        return Err(AlignError::SpanCount {
            spans: spans.len(),
            targets: flat.len(),
        });
    }
    let mut rest = spans.as_slice();
    Ok(words
        .iter()
        .map(|w| {
            let (head, tail) = rest.split_at(w.len());
            rest = tail;
            head.to_vec()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn one_token_one_frame() {
        let e = [-1.0, -0.5];
        let a = forced_align(&Emission::new(&e, 1, 2).unwrap(), &[1]).unwrap();
        assert_eq!(a.path, vec![1]);
    }

    #[test]
    fn repeated_letters_need_a_blank_between() {
        let e = [0.0f32; 3 * 3];
        let err = forced_align(&Emission::new(&e, 3, 3).unwrap(), &[1, 1, 2]).unwrap_err();
        assert!(matches!(
            err,
            AlignError::TooFewFrames {
                frames: 3,
                targets: 3,
                repeats: 1
            }
        ));
    }

    #[test]
    fn a_band_of_frames_keeps_its_rows() {
        let e = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0];
        let em = Emission::new(&e, 3, 2).unwrap();
        let band = em.frames_between(1, 9);
        assert_eq!((band.frames(), band.at(0, 1), band.at(1, 0)), (2, 3.0, 4.0));
        assert_eq!(em.frames_between(5, 2).frames(), 0);
    }

    #[test]
    fn blank_target_is_rejected() {
        let e = [0.0f32; 4];
        assert!(forced_align(&Emission::new(&e, 2, 2).unwrap(), &[0]).is_err());
    }
}
