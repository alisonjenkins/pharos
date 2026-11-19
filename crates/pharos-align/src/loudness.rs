//! How loud the separated vocals are over the song, so a word's start that
//! whisper put back over a silence can be told from a word held that long.

/// Long enough to hold a few cycles of the lowest sung pitch.
const FRAME_S: f64 = 0.02;
/// The song's loud singing: the level 95% of its frames are quieter than.
const LOUD_PERCENTILE: f64 = 0.95;
/// A frame quieter than this share of the loud level, 20 dB down, is
/// silence. In one song the spans whisper stretched over a gap were 69-100%
/// below it and the spans sung through 1-32%; 10 dB or 26 dB moved neither.
const FLOOR_OF_LOUD: f32 = 0.1;
/// A span with more of its frames than this below the floor is silent.
const SILENT_SHARE: f64 = 0.5;

/// Per-frame RMS of the vocals. With no frames, as for a transcript read
/// back from JSON, no span is silent and every start is kept.
#[derive(Debug, Clone, Default)]
pub struct Loudness {
    rms: Vec<f32>,
    floor: f32,
}

impl Loudness {
    /// Frames `samples`, mono at `rate` Hz.
    pub fn of(samples: &[f32], rate: u32) -> Self {
        let frame = ((f64::from(rate) * FRAME_S) as usize).max(1);
        let rms: Vec<f32> = samples
            .chunks_exact(frame)
            .map(|f| (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt())
            .collect();
        let mut sorted = rms.clone();
        sorted.sort_by(f32::total_cmp);
        let loud = sorted
            .get(
                ((sorted.len() as f64 * LOUD_PERCENTILE) as usize)
                    .min(sorted.len().saturating_sub(1)),
            )
            .copied()
            .unwrap_or(0.0);
        Self {
            rms,
            floor: loud * FLOOR_OF_LOUD,
        }
    }

    /// Whether most of `start_s..end_s` is below the silence floor. A span
    /// with no frames, or vocals with no loud singing, is not silent.
    pub fn silent(&self, start_s: f64, end_s: f64) -> bool {
        if self.floor <= 0.0 {
            return false;
        }
        let at = |s: f64| ((s.max(0.0) / FRAME_S) as usize).min(self.rms.len());
        let frames = &self.rms[at(start_s)..at(end_s).max(at(start_s))];
        if frames.is_empty() {
            return false;
        }
        let quiet = frames.iter().filter(|&&r| r < self.floor).count();
        quiet as f64 > frames.len() as f64 * SILENT_SHARE
    }
}

/// Vocals of `(seconds, loud)` spans in order: a loud tone or silence.
#[cfg(test)]
pub(crate) fn test_vocals(spans: &[(f64, bool)], rate: u32) -> Vec<f32> {
    spans
        .iter()
        .flat_map(|&(s, loud)| {
            let n = (s * f64::from(rate)) as usize;
            (0..n).map(move |k| {
                if loud {
                    0.5 * (k as f32 * 0.07).sin()
                } else {
                    0.0
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn vocals(spans: &[(f64, bool)]) -> Vec<f32> {
        test_vocals(spans, RATE)
    }

    #[test]
    fn a_gap_in_the_singing_is_silent_and_the_singing_is_not() {
        let l = Loudness::of(&vocals(&[(4.0, false), (6.0, true)]), RATE);
        assert!(l.silent(0.0, 4.0));
        assert!(!l.silent(4.0, 10.0));
        // Mostly gap, the end of it sung.
        assert!(l.silent(1.0, 5.0));
        assert!(!l.silent(3.0, 7.0));
    }

    #[test]
    fn with_no_vocals_nothing_is_silent() {
        let none = Loudness::default();
        assert!(!none.silent(0.0, 4.0));
        let quiet = Loudness::of(&vocals(&[(4.0, false)]), RATE);
        assert!(!quiet.silent(0.0, 4.0));
        let l = Loudness::of(&vocals(&[(4.0, false), (6.0, true)]), RATE);
        assert!(!l.silent(30.0, 40.0));
    }
}
