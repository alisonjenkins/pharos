//! Whether the lyric text is what was sung (B1479, V1468): the share of
//! the lyric's word pairs the recogniser heard, and how many words it heard.
//! A wrong lyric (another song's text under this song's name) scores near
//! zero, as does a song the recogniser heard little of, so the two are told
//! apart by `heard_words`.

use pharos_core::lyrics_text::{bigram_recall, words};

use serde::Serialize;

use crate::placement::Fallback;
use crate::plan::Line;
use crate::refine::MIN_ANCHOR_SHARE;
use crate::transcript::Word;

/// The text recall of every song with a transcript (V1468).
pub const TEXT_RECALL: &str = "pharos_aligner_text_recall";
/// Tenths: the wrong-lyric songs measured near 0.1, matching ones near 0.9.
pub const TEXT_RECALL_BUCKETS: &[f64] = &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];
/// Below this recall the lyric is not what was sung: a wrong lyric measured
/// 0.107 and the right one 0.917 on the same transcript.
pub const MAX_MISMATCH_RECALL: f64 = 0.3;
/// The fewest words heard for a low recall to mean a wrong lyric rather
/// than a song the recogniser barely heard; a sung song gives hundreds.
pub const MIN_MISMATCH_HEARD_WORDS: usize = 100;

/// What the recogniser heard against the lyric's text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextCheck {
    /// [`bigram_recall`] of the lyric against the transcript; `None` when
    /// the lyric cannot be judged that way.
    pub text_recall: Option<f64>,
    /// Words in the transcript.
    pub heard_words: usize,
    /// The transcript's text, as the recogniser wrote it.
    pub heard: String,
}

/// What the answer says about the text check when a song fell back, so the
/// server can tell a wrong lyric from a song the stages could not place.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Check {
    /// The fallback's label.
    pub fallback: &'static str,
    pub text_recall: Option<f64>,
    pub heard_words: usize,
    pub heard: String,
}

impl TextCheck {
    pub fn new(lines: &[Line], heard: &[Word]) -> Self {
        // Each word is whole, its pieces joined already; only a segment's
        // first word may lack a leading space, and it is a word of its own.
        let mut text = String::new();
        for w in heard {
            if !text.is_empty() && !w.text.starts_with(char::is_whitespace) {
                text.push(' ');
            }
            text.push_str(&w.text);
        }
        Self {
            text_recall: bigram_recall(lines.iter().map(|l| l.text.as_str()), &text),
            heard_words: words(&text).len(),
            heard: text,
        }
    }

    /// Whether a song anchored at `anchor_share` was sung to another lyric
    /// (V1468): few lines anchored, little of the text heard, and enough
    /// heard for that to mean something.
    pub fn mismatch(&self, anchor_share: f64) -> bool {
        anchor_share < MIN_ANCHOR_SHARE && self.text_mismatch()
    }

    /// Whether the text alone says another lyric was sung: little of it
    /// heard, and enough heard for that to mean something. A song whose
    /// later stages failed has no anchor share, and is judged by this.
    pub fn text_mismatch(&self) -> bool {
        self.heard_words >= MIN_MISMATCH_HEARD_WORDS
            && self.text_recall.is_some_and(|r| r < MAX_MISMATCH_RECALL)
    }

    /// The answer's account of this check for a song that fell back because of `why`.
    pub fn answer(&self, why: Fallback) -> Check {
        Check {
            fallback: why.label(),
            text_recall: self.text_recall,
            heard_words: self.heard_words,
            heard: self.heard.clone(),
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

    /// A segment's first word, written with no leading space, is not run
    /// into the last word of the segment before. Disarm check: with the
    /// words joined as written, this fails.
    #[test]
    fn a_segment_s_first_word_stays_a_word_of_its_own() {
        let mut h = heard("grey gulls");
        h.push(Word {
            text: "wheel".into(),
            start_s: 0.0,
            end_s: 0.1,
            prob: 0.9,
            silent_lead: false,
        });
        let got = TextCheck::new(&[line("grey gulls wheel")], &h);
        assert_eq!(got.text_recall, Some(1.0));
        assert_eq!(got.heard_words, 3);
    }

    fn check(text_recall: Option<f64>, heard_words: usize) -> TextCheck {
        TextCheck {
            text_recall,
            heard_words,
            heard: String::new(),
        }
    }

    /// V1468's thresholds, on the measured cases.
    #[test]
    fn a_wrong_lyric_is_a_mismatch_and_nothing_else_is() {
        let cases = [
            ("wrong lyric", 0.081, Some(0.107), 239, true),
            ("right lyric, few anchors", 0.081, Some(0.917), 239, false),
            ("episode job, high recall", 0.083, Some(0.85), 900, false),
            ("sparse song", 0.081, Some(0.05), 99, false),
            ("enough heard at the edge", 0.081, Some(0.05), 100, true),
            ("well anchored", 0.4, Some(0.107), 239, false),
            ("recall at the edge", 0.081, Some(0.3), 239, false),
            ("lyric not judged", 0.081, None, 239, false),
        ];
        for (name, share, recall, heard, want) in cases {
            assert_eq!(check(recall, heard).mismatch(share), want, "{name}");
        }
    }

    #[test]
    fn the_answer_check_carries_the_fallback_and_what_was_heard() -> serde_json::Result<()> {
        let lines = [line("grey gulls wheel")];
        let c = TextCheck::new(&lines, &heard("the lanterns swing"));
        let got = serde_json::to_value(c.answer(Fallback::LyricsMismatch))?;
        assert_eq!(
            got,
            serde_json::json!({
                "fallback": "lyrics_mismatch",
                "text_recall": 0.0,
                "heard_words": 3,
                "heard": " the lanterns swing",
            })
        );
        Ok(())
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
        let judged = check(Some(0.25), 120);
        assert_eq!(recorded(&judged), [0.25]);
        let unjudged = check(None, 120);
        assert!(recorded(&unjudged).is_empty());
    }
}
