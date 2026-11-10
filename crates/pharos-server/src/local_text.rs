//! B992 — which local episode text a provider's record replaces.
//!
//! A scanned episode's title is sometimes not a title at all (a disc label,
//! a release name, its own file stem), and its air date can be the container
//! tag's rip date. Both must lose to the provider's record; this module names
//! the reasons, classifies a title, and records each replacement.

use pharos_core::MediaId;

/// Why a local value was replaced by the provider's. The label is a metric
/// contract: bounded and stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceReason {
    /// A disc label such as `SHOW A - S1 D2` or `SHOW A - SEASON 4 DISC 1`.
    DiscLabel,
    /// A release-style file name: dotted words plus a resolution or source tag.
    ReleaseName,
    /// The title is the file's own stem.
    FileStem,
    /// The air date is the container tag's, not an NFO's.
    ContainerDate,
}

impl ReplaceReason {
    /// The metadata field this reason replaces.
    pub fn field(self) -> &'static str {
        match self {
            Self::DiscLabel | Self::ReleaseName | Self::FileStem => "title",
            Self::ContainerDate => "premiere_date",
        }
    }

    /// Stable, bounded-cardinality metric label, asserted distinct in a test.
    pub fn label(self) -> &'static str {
        match self {
            Self::DiscLabel => "disc_label",
            Self::ReleaseName => "release_name",
            Self::FileStem => "file_stem",
            Self::ContainerDate => "container_date",
        }
    }
}

/// Count and log one replacement. `old` is the value that lost.
pub fn record_replaced(media_id: MediaId, reason: ReplaceReason, old: &str) {
    metrics::counter!(
        "pharos_episode_local_text_replaced_total",
        "field" => reason.field(),
        "reason" => reason.label()
    )
    .increment(1);
    tracing::info!(
        media.id = media_id,
        field = reason.field(),
        reason = reason.label(),
        old_value = old,
        "T9 metadata backfill: the record's value replaces a local one that is not curated"
    );
}

/// Letters and digits only, lower-cased: how a title and a file stem compare.
fn fold(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Why `title` is not an episode's name, or `None` when it may be one.
/// `stem` is the episode file's stem.
pub fn junk_title(title: &str, stem: &str) -> Option<ReplaceReason> {
    let folded = fold(title);
    if !folded.is_empty() && folded == fold(stem) {
        Some(ReplaceReason::FileStem)
    } else if is_disc_label(title) {
        Some(ReplaceReason::DiscLabel)
    } else if is_release_name(title) {
        Some(ReplaceReason::ReleaseName)
    } else {
        None
    }
}

/// `S<n> D<n>` or `DISC <n>`, case-insensitive, as whole words.
fn is_disc_label(title: &str) -> bool {
    let b = title.to_ascii_lowercase().into_bytes();
    (0..b.len()).any(|i| {
        let starts_word = i == 0 || b.get(i - 1).is_some_and(|c| !c.is_ascii_alphanumeric());
        starts_word && (season_disc_at(&b, i) || disc_at(&b, i))
    })
}

/// Index after the digits starting at `i`, when there is at least one.
fn digits_end(b: &[u8], i: usize) -> Option<usize> {
    let n = b
        .get(i..)?
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count();
    (n > 0).then_some(i + n)
}

fn word_ends_at(b: &[u8], i: usize) -> bool {
    !b.get(i).is_some_and(|c| c.is_ascii_alphanumeric())
}

fn season_disc_at(b: &[u8], i: usize) -> bool {
    if b.get(i) != Some(&b's') {
        return false;
    }
    let Some(mut at) = digits_end(b, i + 1) else {
        return false;
    };
    if b.get(at) == Some(&b' ') {
        at += 1;
    }
    b.get(at) == Some(&b'd') && digits_end(b, at + 1).is_some_and(|end| word_ends_at(b, end))
}

fn disc_at(b: &[u8], i: usize) -> bool {
    if b.get(i..i + 4) != Some(b"disc".as_slice()) {
        return false;
    }
    let mut at = i + 4;
    if b.get(at) == Some(&b' ') {
        at += 1;
    }
    digits_end(b, at).is_some_and(|end| word_ends_at(b, end))
}

/// Dotted words (no spaces, at least two dots) with a resolution or source tag.
fn is_release_name(title: &str) -> bool {
    let t = title.trim();
    !t.contains(char::is_whitespace)
        && t.matches('.').count() >= 2
        && t.split(['.', '_']).any(is_release_tag)
}

fn is_release_tag(word: &str) -> bool {
    const SOURCES: [&str; 12] = [
        "web-dl", "webrip", "bluray", "bdrip", "brrip", "hdtv", "dvdrip", "x264", "x265", "h264",
        "h265", "hevc",
    ];
    let w = word.to_ascii_lowercase();
    let resolution = w
        .strip_suffix(['p', 'i'])
        .is_some_and(|n| (3..=4).contains(&n.len()) && n.bytes().all(|c| c.is_ascii_digit()));
    resolution
        || SOURCES.iter().any(|tag| {
            w.strip_prefix(tag)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_junk_titles_by_reason() {
        use ReplaceReason::{DiscLabel, FileStem, ReleaseName};
        let stem = "Show A - S1 E02 - 1080p";
        let table: [(&str, Option<ReplaceReason>); 18] = [
            ("SHOW A - S1 D2", Some(DiscLabel)),
            ("Show A - S12 D10", Some(DiscLabel)),
            ("Show A - S1D2", Some(DiscLabel)),
            ("SHOW A - SEASON 4 DISC 1", Some(DiscLabel)),
            ("show a - disc 3", Some(DiscLabel)),
            ("Show A DISC2", Some(DiscLabel)),
            (
                "Show.A.S02E06.Part.One.1080p.WEBRip.10Bit.DDP5.1.HEVC-GRP",
                Some(ReleaseName),
            ),
            ("Show.A.S02E06.720p.HDTV.x264-GRP", Some(ReleaseName)),
            ("Show.A.S02E06.WEB-DL.AAC", Some(ReleaseName)),
            ("show a - s1 e02 - 1080p", Some(FileStem)),
            ("Show.A.S1.E03.1080p", Some(ReleaseName)),
            // Curated titles stay.
            ("The Distant Signal", None),
            ("Disclosure", None),
            ("Discs of Fire", None),
            ("Blue Ray", None),
            ("Mrs. A. Smith", None),
            ("Session 5 Dilemma", None),
            ("1080p", None),
        ];
        for (title, want) in table {
            assert_eq!(junk_title(title, stem), want, "{title}");
        }
    }

    #[test]
    fn an_empty_title_is_not_matched_to_an_empty_stem() {
        assert_eq!(junk_title("", ""), None);
    }

    #[test]
    fn reasons_have_distinct_stable_labels() {
        let all = [
            ReplaceReason::DiscLabel,
            ReplaceReason::ReleaseName,
            ReplaceReason::FileStem,
            ReplaceReason::ContainerDate,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), all.len());
        assert_eq!(ReplaceReason::ContainerDate.field(), "premiere_date");
        assert_eq!(ReplaceReason::DiscLabel.field(), "title");
    }
}
