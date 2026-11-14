//! The recogniser's words with their times (spec 042), and a reader for a
//! transcript saved as JSON, so matching and refining can be built and
//! checked before the recogniser itself is in Rust.

use std::path::Path;

/// One word as the recogniser heard it.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Word {
    /// The word as written, often with a leading space.
    #[serde(rename = "w")]
    pub text: String,
    /// Seconds from the start of the song.
    #[serde(rename = "s")]
    pub start_s: f64,
    #[serde(rename = "e")]
    pub end_s: f64,
    /// The recogniser's confidence, 0 to 1.
    #[serde(rename = "p", default)]
    pub prob: f64,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct Transcript {
    pub words: Vec<Word>,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("reading transcript {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing transcript: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(
        "word {index} {text:?} has a time that is negative or not finite: {start_s} to {end_s}"
    )]
    BadTime {
        index: usize,
        text: String,
        start_s: f64,
        end_s: f64,
    },
}

impl Transcript {
    /// A transcript from `{"words": [{"w", "s", "e", "p"}, ...]}`; other fields are ignored.
    pub fn from_json(json: &str) -> Result<Self, TranscriptError> {
        let t: Self = serde_json::from_str(json)?;
        if let Some((index, w)) = t.words.iter().enumerate().find(|(_, w)| {
            !(w.start_s.is_finite() && w.end_s.is_finite() && w.start_s >= 0.0 && w.end_s >= 0.0)
        }) {
            return Err(TranscriptError::BadTime {
                index,
                text: w.text.clone(),
                start_s: w.start_s,
                end_s: w.end_s,
            });
        }
        Ok(t)
    }

    pub fn read(path: &Path) -> Result<Self, TranscriptError> {
        let json = std::fs::read_to_string(path).map_err(|source| TranscriptError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_json(&json)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn reads_words_and_ignores_other_fields() {
        let t = Transcript::from_json(
            r#"{"words":[{"w":" Paper","s":1.5,"e":1.9,"p":0.8},{"w":" boats","s":2.0,"e":2.4}],
                "segments":[],"sec":3.1}"#,
        )
        .unwrap();
        assert_eq!(t.words.len(), 2);
        assert_eq!(t.words[0].text, " Paper");
        assert_eq!((t.words[0].start_s, t.words[0].end_s), (1.5, 1.9));
        assert_eq!(t.words[1].prob, 0.0);
    }

    #[test]
    fn a_negative_time_is_named() {
        let e =
            Transcript::from_json(r#"{"words":[{"w":"x","s":-1.0,"e":0.5,"p":1}]}"#).unwrap_err();
        assert!(
            matches!(e, TranscriptError::BadTime { index: 0, .. }),
            "{e}"
        );
    }

    #[test]
    fn missing_words_is_a_parse_error() {
        assert!(matches!(
            Transcript::from_json("{}"),
            Err(TranscriptError::Parse(_))
        ));
    }
}
