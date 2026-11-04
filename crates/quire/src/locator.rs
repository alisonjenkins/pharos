//! Reading positions that survive a change of window, font or page size.

use std::fmt;
use std::str::FromStr;

/// A place in a reflowable book: the reading-order index and an offset into
/// that section's [`crate::Section::text`], in UTF-16 code units, the unit DOM
/// `Range` offsets use. Written as `epub1:<spine>:<offset>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Locator {
    pub spine: u32,
    pub offset: u32,
}

const PREFIX: &str = "epub1";

impl fmt::Display for Locator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}:{}:{}", self.spine, self.offset)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a quire locator: {0:?}")]
pub struct ParseLocatorError(String);

impl FromStr for Locator {
    type Err = ParseLocatorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || ParseLocatorError(s.chars().take(64).collect());
        let mut parts = s.split(':');
        if parts.next() != Some(PREFIX) {
            return Err(bad());
        }
        // Digits only, so one place has one spelling (`+4` would parse).
        let mut number = || {
            parts
                .next()
                .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|p| p.parse().ok())
                .ok_or_else(bad)
        };
        let spine = number()?;
        let offset = number()?;
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(Self { spine, offset })
    }
}

/// UTF-16 offset of the `char_index`th character of `text`, clamped to its end.
pub fn utf16_offset(text: &str, char_index: usize) -> usize {
    text.chars().take(char_index).map(char::len_utf16).sum()
}

/// Index of the character holding UTF-16 offset `utf16`, clamped to the
/// character count. An offset inside a surrogate pair names that character.
pub fn char_index(text: &str, utf16: usize) -> usize {
    let mut units = 0usize;
    for (i, c) in text.chars().enumerate() {
        units = units.saturating_add(c.len_utf16());
        if units > utf16 {
            return i;
        }
    }
    text.chars().count()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn round_trips_through_its_string() {
        let l = Locator {
            spine: 4,
            offset: 12345,
        };
        assert_eq!(l.to_string(), "epub1:4:12345");
        assert_eq!("epub1:4:12345".parse::<Locator>().unwrap(), l);
        assert!("epub1:4".parse::<Locator>().is_err());
        assert!("epub2:4:1".parse::<Locator>().is_err());
        assert!("epub1:4:1:9".parse::<Locator>().is_err());
        assert!("epub1:-1:1".parse::<Locator>().is_err());
        assert!("epub1:+4:1".parse::<Locator>().is_err());
        assert!("epub1:4: 1".parse::<Locator>().is_err());
    }

    #[test]
    fn offsets_count_utf16_units_past_the_basic_plane() {
        let text = "a😀b";
        assert_eq!(utf16_offset(text, 2), 3);
        assert_eq!(char_index(text, 3), 2);
        assert_eq!(char_index(text, 2), 1, "inside the pair names the emoji");
        assert_eq!(char_index(text, 99), 3);
    }

    proptest! {
        #[test]
        fn char_and_utf16_offsets_invert(text in "\\PC{0,40}", i in 0usize..50) {
            let i = i.min(text.chars().count());
            prop_assert_eq!(char_index(&text, utf16_offset(&text, i)), i);
        }
    }
}
