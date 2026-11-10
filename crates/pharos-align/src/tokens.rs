//! The MMS_FA label set: CTC blank, 27 letters and the star wildcard.

use crate::AlignError;

/// CTC blank: no label at this frame.
pub const BLANK: u32 = 0;
/// MMS_FA's wildcard label, matching any audio that is not the transcript.
/// The model has no output for it; its emission column is log 1 = 0.
pub const STAR: u32 = 28;
/// Emission width with the star column appended.
pub const CLASSES: usize = 29;
/// The star's spelling as a token.
pub const STAR_TOKEN: &str = "*";

/// `torchaudio.pipelines.MMS_FA.get_dict()`, index order; index 0 is the blank.
const LABELS: &str = "-aienoutsrmkldghybpwcvjzf'qx*";

/// The label index of one character, or `None` if MMS_FA has no such label.
pub fn label(c: char) -> Option<u32> {
    if c == '-' {
        return None;
    }
    LABELS
        .chars()
        .position(|l| l == c)
        .and_then(|i| u32::try_from(i).ok())
}

/// One label sequence per token, as `bundle.get_tokenizer()` builds them.
pub fn tokenize<S: AsRef<str>>(tokens: &[S]) -> Result<Vec<Vec<u32>>, AlignError> {
    tokens
        .iter()
        .map(|t| {
            let t = t.as_ref();
            t.chars()
                .map(|c| {
                    label(c).ok_or_else(|| AlignError::UnknownCharacter {
                        character: c,
                        token: t.to_string(),
                    })
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_the_alphabet_once() {
        assert_eq!(LABELS.chars().count(), CLASSES);
        for c in "abcdefghijklmnopqrstuvwxyz'".chars() {
            assert!(label(c).is_some(), "{c}");
        }
        assert_eq!(label('*'), Some(STAR));
    }

    #[test]
    fn blank_is_not_a_token_character() {
        assert_eq!(label('-'), None);
        assert!(tokenize(&["a-b"]).is_err());
    }
}
