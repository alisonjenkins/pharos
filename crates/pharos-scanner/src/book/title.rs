//! A book's title when the file names none: the file stem, cleaned.
//!
//! A book with no embedded title is listed under its file stem. Stems arrive
//! with download-site stamps, copy counters and separators standing in for
//! spaces, none of which belong in a title. [`clean_stem_title`] removes those
//! and nothing else: version numbers, years, editions and every other word of
//! a real title are kept. It never splits run-together words (no dictionary)
//! and never returns an empty title.
//!
//! [`stem_title`] is the scan's entry point. It records each stem it changed in
//! `pharos_book_title_cleaned_total` (no labels) and in an info log carrying
//! the `before` and `after` values, which are unbounded and so stay out of the
//! metric (V1390).

use std::sync::LazyLock;

use regex::Regex;

/// Compile `pattern` once. A pattern that fails to compile is logged and the
/// rule is skipped, so a typo here degrades to an uncleaned title, never a
/// panic (V6).
fn rule(pattern: &str) -> Option<Regex> {
    Regex::new(pattern)
        .map_err(|e| tracing::error!(error = %e, pattern, "invalid book title rule"))
        .ok()
}

/// Trailing patterns removed one at a time, until none matches.
static TRAILING: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Download-site stamp: `... eBook www SomeSite se` or `www.site.se`.
        r"(?i)(?:\s+ebook)?\s+www(?:\s|\.).*$",
        // A bracketed domain: `(z-lib.org)`, `[libsite.com]`. The last label
        // must be letters so `(v2.0)` is not taken for one.
        r"(?i)\s*[(\[][\w-]+(?:\.[\w-]+)*\.[a-z]{2,}[)\]]$",
        // Purchaser stamp: `(for Mr A B Smith)`.
        r"(?i)\s*[(\[]for\s[^)\]]+[)\]]$",
        // Copy counter: `(1)`, `(12)`.
        r"\s*\(\d{1,2}\)$",
        // `- Copy`, `- Copy (2)`, ` copy`.
        r"(?i)\s*-\s*copy(?:\s*\(\d{1,2}\))?$",
        // An ISBN or id left on the end: ten or more digits, possibly an X.
        r"\s+\d{10,13}[xX]?$",
        // Release tag meant for video.
        r"(?i)\s+(?:web-?dl|web-?rip)$",
    ]
    .iter()
    .filter_map(|p| rule(p))
    .collect()
});

static WHITESPACE: LazyLock<Option<Regex>> = LazyLock::new(|| rule(r"\s+"));

/// Whether the hyphens inside words (`a-b`) stand for spaces: at least two of
/// them, and more of them than real spaces. `Spider-Man Returns` and
/// `Test-Driven Development with Mutation-Testing` keep theirs;
/// `making-a-paper-kite` loses them.
fn hyphens_are_separators(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let hyphens = chars
        .windows(3)
        .filter(|w| matches!(w, [a, '-', b] if a.is_alphanumeric() && b.is_alphanumeric()))
        .count();
    let spaces = chars.iter().filter(|c| c.is_whitespace()).count();
    hyphens >= 2 && hyphens > spaces
}

fn collapse(s: &str) -> String {
    let joined = match WHITESPACE.as_ref() {
        Some(re) => re.replace_all(s, " ").into_owned(),
        None => s.to_string(),
    };
    joined.trim().to_string()
}

/// The stem with separators spaced and download, copy and purchaser junk
/// removed. Returns the stem unchanged when cleaning would leave nothing.
pub fn clean_stem_title(stem: &str) -> String {
    let mut s = stem.replace('_', " ");
    if hyphens_are_separators(&s) {
        // Only hyphens between two word characters; ` - ` stays.
        let chars: Vec<char> = s.chars().collect();
        s = chars
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let word_at = |j: Option<usize>| {
                    j.and_then(|j| chars.get(j))
                        .is_some_and(|c| c.is_alphanumeric())
                };
                if c == '-' && word_at(i.checked_sub(1)) && word_at(i.checked_add(1)) {
                    ' '
                } else {
                    c
                }
            })
            .collect();
    }
    let mut s = collapse(&s);
    loop {
        let before = s.len();
        for re in TRAILING.iter() {
            let cut = re.replace(&s, "");
            let cut = cut.trim_end_matches([' ', '-', '.']).trim();
            // A rule that would eat the whole title is not a cleanup.
            if !cut.is_empty() && cut.len() < s.len() {
                s = cut.to_string();
            }
        }
        if s.len() == before {
            break;
        }
    }
    if s.is_empty() {
        stem.to_string()
    } else {
        s
    }
}

/// The title for a book whose file named none, recording a cleanup.
pub fn stem_title(stem: &str) -> String {
    let after = clean_stem_title(stem);
    if after != stem {
        metrics::counter!("pharos_book_title_cleaned_total").increment(1);
        tracing::info!(before = stem, after = %after, "book: stem title cleaned");
    }
    after
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn stems_are_cleaned_table() {
        let cases: &[(&str, &str)] = &[
            // Separators.
            ("learning_rust_the_hard_way", "learning rust the hard way"),
            ("making-a-paper-kite", "making a paper kite"),
            ("Title_ Subtitle Here", "Title Subtitle Here"),
            // Download-site stamps.
            ("Learning Go eBook www SomeSite se", "Learning Go"),
            ("Learning Go www.somesite.se", "Learning Go"),
            ("A Book Title (z-lib.org)", "A Book Title"),
            ("A Book Title [libsite.com]", "A Book Title"),
            // Trailing id.
            ("making-a-paper-kite 1234509876", "making a paper kite"),
            ("A Book Title 9781234567897", "A Book Title"),
            // Copy counters.
            ("A Book Title (1)", "A Book Title"),
            ("A Book Title (2)", "A Book Title"),
            ("A Book Title - Copy", "A Book Title"),
            ("A Book Title - Copy (2)", "A Book Title"),
            // Purchaser stamp, then a counter beneath it.
            ("Some Title (for Mr A B Smith)", "Some Title"),
            ("Some Title (1) (for Mr A B Smith)", "Some Title"),
            // Video release tag.
            ("Some Title WEB-DL", "Some Title"),
            // Real content stays.
            ("Taming Editors 4.1", "Taming Editors 4.1"),
            ("Taming Editors 4 1", "Taming Editors 4 1"),
            ("Some Title 2nd Edition", "Some Title 2nd Edition"),
            ("Some Title 2019", "Some Title 2019"),
            ("Some Title (2019)", "Some Title (2019)"),
            ("Some Title (v2.0)", "Some Title (v2.0)"),
            ("Spider-Man Returns", "Spider-Man Returns"),
            (
                "Test-Driven Development with Mutation-Testing",
                "Test-Driven Development with Mutation-Testing",
            ),
            ("Part One - The Start", "Part One - The Start"),
            (
                "pythonmachinelearningcookbook",
                "pythonmachinelearningcookbook",
            ),
            // Never empty.
            ("(1)", "(1)"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(&clean_stem_title(input), want, "for {input:?}");
        }
    }

    #[test]
    fn cleaning_twice_changes_nothing_more() {
        for input in [
            "learning_rust_the_hard_way",
            "Some Title (1) (for Mr A B Smith)",
            "Learning Go eBook www SomeSite se",
        ] {
            let once = clean_stem_title(input);
            assert_eq!(clean_stem_title(&once), once, "for {input:?}");
        }
    }

    fn cleaned_count(snapshotter: &pharos_test_metrics::CumulativeSnapshotter) -> u64 {
        use metrics_util::debugging::DebugValue;
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_book_title_cleaned_total")
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(n) => n,
                _ => 0,
            })
            .sum()
    }

    /// A cleaned stem is counted; a stem that was already clean is not.
    #[test]
    fn only_a_changed_stem_is_counted() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        assert_eq!(stem_title("Already Clean"), "Already Clean");
        assert_eq!(cleaned_count(&snapshotter), 0);
        assert_eq!(stem_title("a_book_title (1)"), "a book title");
        assert_eq!(cleaned_count(&snapshotter), 1);
    }
}
