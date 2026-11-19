//! Whether the lyric text is what was sung (B1479, V1468): the share of
//! the lyric's word pairs the recogniser heard, and how many words it heard.
//! A wrong lyric (another song's text under this song's name) scores near
//! zero, as does a song the recogniser heard little of, so the two are told
//! apart by `heard_words`.

use pharos_core::lyrics_text::{bigram_recall, words};

use crate::plan::Line;
use crate::transcript::Word;

/// The text recall of every song with a transcript (V1468).
pub const TEXT_RECALL: &str = "pharos_aligner_text_recall";
/// Tenths: the wrong-lyric songs measured near 0.1, matching ones near 0.9.
pub const TEXT_RECALL_BUCKETS: &[f64] = &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];

/// What the recogniser heard against the lyric's text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextCheck {
    /// [`bigram_recall`] of the lyric against the transcript; `None` when
    /// the lyric cannot be judged that way.
    pub text_recall: Option<f64>,
    /// Words in the transcript.
    pub heard_words: usize,
}

impl TextCheck {
    pub fn new(lines: &[Line], heard: &[Word]) -> Self {
        let text: String = heard.iter().map(|w| w.text.as_str()).collect();
        Self {
            text_recall: bigram_recall(lines.iter().map(|l| l.text.as_str()), &text),
            heard_words: words(&text).len(),
        }
    }

    /// Records the recall in [`TEXT_RECALL`]; a lyric not judged records nothing.
    pub fn record(&self) {
        if let Some(r) = self.text_recall {
            metrics::histogram!(TEXT_RECALL).record(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    fn line(text: &str) -> Line {
        Line {
            start_ms: 0,
            end_ms: 1_000,
            text: text.into(),
        }
    }

    fn heard(text: &str) -> Vec<Word> {
        text.split_whitespace()
            .map(|w| Word {
                text: format!(" {w}"),
                start_s: 0.0,
                end_s: 0.1,
                prob: 0.9,
                silent_lead: false,
            })
            .collect()
    }

    #[test]
    fn a_heard_lyric_scores_high_and_counts_the_words_heard() {
        let lines = [line("grey gulls wheel"), line("low tide hums")];
        let got = TextCheck::new(&lines, &heard("grey gulls wheel low tide hums"));
        assert_eq!(got.text_recall, Some(1.0));
        assert_eq!(got.heard_words, 6);
    }

    #[test]
    fn a_wrong_lyric_scores_zero() {
        let lines = [line("grey gulls wheel"), line("low tide hums")];
        let got = TextCheck::new(&lines, &heard("the lanterns swing in the orchard"));
        assert_eq!(got.text_recall, Some(0.0));
        assert_eq!(got.heard_words, 6);
    }

    #[test]
    fn sub_word_pieces_join_into_the_words_heard() {
        let mut h = heard("grey gulls");
        h.push(Word {
            text: "wheel".into(),
            start_s: 0.0,
            end_s: 0.1,
            prob: 0.9,
            silent_lead: false,
        });
        let got = TextCheck::new(&[line("grey gullswheel")], &h);
        assert_eq!(got.text_recall, Some(1.0));
        assert_eq!(got.heard_words, 2);
    }

    fn recorded(check: &TextCheck) -> Vec<f64> {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || check.record());
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == TEXT_RECALL)
            .flat_map(|(.., v)| match v {
                DebugValue::Histogram(h) => h.into_iter().map(|v| v.0).collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn the_recall_is_recorded_and_an_unjudged_lyric_is_not() {
        let judged = TextCheck {
            text_recall: Some(0.25),
            heard_words: 120,
        };
        assert_eq!(recorded(&judged), [0.25]);
        let unjudged = TextCheck {
            text_recall: None,
            heard_words: 120,
        };
        assert!(recorded(&unjudged).is_empty());
    }
}
