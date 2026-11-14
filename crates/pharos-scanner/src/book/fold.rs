//! One item per book: the formats of one book side by side in one folder
//! (`Widget Cookbook.epub`, `Widget Cookbook.pdf`) and its exact copies
//! (`Widget Cookbook (1).epub`) fold into a single item whose other files are
//! its versions, as a film's copies do (V1391).
//!
//! Two files fold only when they sit in the same folder, their stems give the
//! same title once cleaned ([`clean_stem_title`], case folded), and their
//! authors agree or one of them names none. The primary is the file the
//! reader opens best ([`FormatRank`]); the others become its versions,
//! labelled by format.
//!
//! Every decision on a pair is counted in
//! `pharos_book_fold_total{outcome, reason}` and logged at info with both
//! paths, the title and both authors. Paths and titles stay in the log line:
//! they are unbounded, so they never become labels.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use pharos_core::BookFormat;

use super::title::clean_stem_title;
use super::{format_for_path, guard_parser, kindle};

/// A primary file and its `(label, path)` versions, as the scanner groups them.
pub(crate) type Group = (PathBuf, Vec<(String, PathBuf)>);

/// How well the reader opens a format, best first: the order a book's files
/// are ranked in for its primary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FormatRank {
    Epub,
    Pdf,
    Comic,
    /// A Kindle file with no DRM, which delivery converts to an epub.
    Kindle,
    /// A Kindle file no client can open (DRM, or one that would not convert).
    Locked,
}

impl FormatRank {
    /// The rank of a file stored as `format` (the format a client can read it
    /// as, so a converting Kindle file is stored as an epub).
    pub fn of(path: &Path, format: BookFormat) -> Self {
        match format {
            BookFormat::Epub if kindle::is_kindle_path(path) => Self::Kindle,
            BookFormat::Epub => Self::Epub,
            BookFormat::Pdf => Self::Pdf,
            BookFormat::Comic => Self::Comic,
            BookFormat::Unreadable => Self::Locked,
        }
    }
}

/// What a book version's picker row is called: `EPUB`, `PDF`, `CBZ`,
/// `Kindle`, or `Kindle (locked)` for one that will not open.
pub fn format_name(path: &Path, format: BookFormat) -> String {
    let ext = || {
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_uppercase)
            .unwrap_or_default()
    };
    match FormatRank::of(path, format) {
        FormatRank::Epub => "EPUB".to_string(),
        FormatRank::Pdf => "PDF".to_string(),
        FormatRank::Comic => ext(),
        FormatRank::Kindle => "Kindle".to_string(),
        FormatRank::Locked if kindle::is_kindle_path(path) => "Kindle (locked)".to_string(),
        FormatRank::Locked => format!("{} (locked)", ext()),
    }
}

/// The container a book version advertises: the format a client reads it as
/// (`epub` for a Kindle file that converts), else the file's own extension.
/// [`super::format_for_extension`] reads it back.
pub fn readable_container(path: &Path, format: BookFormat) -> String {
    match format {
        BookFormat::Epub => "epub".to_string(),
        BookFormat::Pdf => "pdf".to_string(),
        BookFormat::Comic | BookFormat::Unreadable => path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default(),
    }
}

/// What the fold knows of one book file before it is scanned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoldFacts {
    pub(crate) rank: FormatRank,
    pub(crate) author: Option<String>,
    pub(crate) size: Option<u64>,
}

/// Read a book file's rank and author, for the fold only. Uses the raw
/// readers, so nothing here lands on the classify counter: the scan reads the
/// file again and records that. A file that does not parse ranks by its
/// extension and names no author.
pub(crate) fn read_fold_facts(path: &Path) -> Option<FoldFacts> {
    let format = format_for_path(path)?;
    let size = std::fs::metadata(path).ok().map(|m| m.len());
    let (rank, author) = match format {
        BookFormat::Epub => (
            FormatRank::Epub,
            guard_parser(path, format, "epub fold facts", || {
                super::epub::read_epub(path).ok().and_then(|m| m.author)
            })
            .flatten(),
        ),
        BookFormat::Pdf => (
            FormatRank::Pdf,
            guard_parser(path, format, "pdf fold facts", || {
                lopdf::Document::load_metadata(path)
                    .ok()
                    .and_then(|m| m.author)
            })
            .flatten(),
        ),
        BookFormat::Comic => (
            FormatRank::Comic,
            guard_parser(path, format, "comic fold facts", || {
                super::comic::read_comic(path).ok().and_then(|m| m.author)
            })
            .flatten(),
        ),
        BookFormat::Unreadable => {
            match guard_parser(path, format, "kindle fold facts", || {
                kindle::read_kindle(path)
            }) {
                Some(Ok(meta)) => (FormatRank::Kindle, meta.author),
                _ => (FormatRank::Locked, None),
            }
        }
    };
    Some(FoldFacts { rank, author, size })
}

/// What the fold decided for one file against the primary of its title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FoldOutcome {
    /// The file is now a version of the primary.
    Folded,
    /// The file stays its own item.
    KeptApart,
}

impl FoldOutcome {
    pub const ALL: [Self; 2] = [Self::Folded, Self::KeptApart];

    /// The `outcome` label of `pharos_book_fold_total`: a dashboard contract.
    pub fn label(self) -> &'static str {
        match self {
            Self::Folded => "folded",
            Self::KeptApart => "kept_apart",
        }
    }
}

/// Why, on the authors: the only fact that can keep two files of one title
/// in one folder apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FoldReason {
    /// Both files name the same author.
    SameAuthor,
    /// At least one file names no author.
    AuthorUnknown,
    /// The files name different authors: two books that share a title.
    AuthorsDiffer,
}

impl FoldReason {
    pub const ALL: [Self; 3] = [Self::SameAuthor, Self::AuthorUnknown, Self::AuthorsDiffer];

    /// The `reason` label of `pharos_book_fold_total`: a dashboard contract.
    pub fn label(self) -> &'static str {
        match self {
            Self::SameAuthor => "same_author",
            Self::AuthorUnknown => "author_unknown",
            Self::AuthorsDiffer => "authors_differ",
        }
    }

    /// The verdict for two authors: the same once case, punctuation and word
    /// order are set aside (`Smith, Jane` is `Jane Smith`).
    fn of(a: Option<&str>, b: Option<&str>) -> Self {
        let key = |s: &str| {
            let mut words: Vec<String> = s
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(str::to_lowercase)
                .collect();
            words.sort();
            words
        };
        match (a.map(key), b.map(key)) {
            (Some(a), Some(b)) if a.is_empty() || b.is_empty() => Self::AuthorUnknown,
            (Some(a), Some(b)) if a == b => Self::SameAuthor,
            (Some(_), Some(_)) => Self::AuthorsDiffer,
            _ => Self::AuthorUnknown,
        }
    }

    fn outcome(self) -> FoldOutcome {
        match self {
            Self::SameAuthor | Self::AuthorUnknown => FoldOutcome::Folded,
            Self::AuthorsDiffer => FoldOutcome::KeptApart,
        }
    }
}

/// The title two files must share to fold: their folder and their cleaned,
/// case-folded stem. `None` for a file that is not a book.
fn fold_key(path: &Path) -> Option<(PathBuf, String)> {
    format_for_path(path)?;
    let stem = path.file_stem()?.to_str()?;
    let title = clean_stem_title(stem).to_lowercase();
    Some((path.parent()?.to_path_buf(), title))
}

/// Whether the stem carries junk the title cleanup removes (`(1)`,
/// `- Copy`): such a file is the copy, the clean one its original.
fn has_junk_stem(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| clean_stem_title(s) != s)
}

/// How a file ranks for its title's primary, best first: the format the
/// reader opens best, a clean stem before a copy's, the bigger file, then
/// the path.
fn rank_key(path: &Path, facts: &FoldFacts) -> (FormatRank, bool, std::cmp::Reverse<u64>, PathBuf) {
    (
        facts.rank,
        has_junk_stem(path),
        std::cmp::Reverse(facts.size.unwrap_or(0)),
        path.to_path_buf(),
    )
}

/// The books of `groups` that share a [`fold_key`] with another book: only
/// these have their facts read.
pub(crate) fn candidates(groups: &[Group]) -> Vec<PathBuf> {
    let mut by_key: BTreeMap<(PathBuf, String), Vec<&Path>> = BTreeMap::new();
    for (path, _) in groups {
        if let Some(key) = fold_key(path) {
            by_key.entry(key).or_default().push(path);
        }
    }
    by_key
        .into_values()
        .filter(|members| members.len() > 1)
        .flatten()
        .map(Path::to_path_buf)
        .collect()
}

/// One fold decision, as it is counted and logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) primary: PathBuf,
    pub(crate) copy: PathBuf,
    pub(crate) outcome: FoldOutcome,
    pub(crate) reason: FoldReason,
}

/// Fold the books of `groups` that are one book (see the module docs), given
/// each candidate's facts. A candidate with no facts is left as it was.
/// Returns the groups, in their order, less each folded file, and the
/// decision taken on each file that shares a title with a primary.
pub(crate) fn fold(
    groups: Vec<Group>,
    facts: &HashMap<PathBuf, FoldFacts>,
) -> (Vec<Group>, Vec<Decision>) {
    let mut by_key: BTreeMap<(PathBuf, String), Vec<usize>> = BTreeMap::new();
    for (i, (path, _)) in groups.iter().enumerate() {
        if !facts.contains_key(path) {
            continue;
        }
        if let Some(key) = fold_key(path) {
            by_key.entry(key).or_default().push(i);
        }
    }
    let mut into: HashMap<usize, usize> = HashMap::new();
    let mut decisions = Vec::new();
    for members in by_key.into_values().filter(|m| m.len() > 1) {
        let mut ranked: Vec<(usize, &Path, &FoldFacts)> = members
            .iter()
            .filter_map(|&i| {
                let path = groups.get(i)?.0.as_path();
                Some((i, path, facts.get(path)?))
            })
            .collect();
        ranked.sort_by_key(|(_, path, f)| rank_key(path, f));
        // The best file left is a primary; the files it cannot take (another
        // author's book of the same title) wait for the next round.
        while let Some((&(primary, primary_path, primary_facts), rest)) = ranked.split_first() {
            let mut apart = Vec::new();
            for &(member, path, f) in rest {
                let reason = FoldReason::of(primary_facts.author.as_deref(), f.author.as_deref());
                decisions.push(Decision {
                    primary: primary_path.to_path_buf(),
                    copy: path.to_path_buf(),
                    outcome: reason.outcome(),
                    reason,
                });
                match reason.outcome() {
                    FoldOutcome::Folded => {
                        into.insert(member, primary);
                    }
                    FoldOutcome::KeptApart => apart.push((member, path, f)),
                }
            }
            ranked = apart;
        }
    }
    if into.is_empty() {
        return (groups, decisions);
    }
    let mut taken: Vec<Option<Group>> = groups.into_iter().map(Some).collect();
    let mut gathered: BTreeMap<usize, Vec<(String, PathBuf)>> = BTreeMap::new();
    let mut moved: Vec<(usize, usize)> = into.into_iter().collect();
    moved.sort_unstable();
    for (copy, primary) in moved {
        let Some((path, editions)) = taken.get_mut(copy).and_then(Option::take) else {
            continue;
        };
        let primary_path = taken
            .get(primary)
            .and_then(Option::as_ref)
            .map(|g| g.0.clone())
            .unwrap_or_default();
        let label = version_label(&primary_path, &path, facts);
        let entry = gathered.entry(primary).or_default();
        entry.push((label, path));
        entry.extend(editions);
    }
    let out = taken
        .into_iter()
        .enumerate()
        .filter_map(|(i, group)| {
            let (primary, mut alts) = group?;
            alts.extend(gathered.remove(&i).unwrap_or_default());
            Some((primary, alts))
        })
        .collect();
    (out, decisions)
}

/// A folded file's picker name: its format, and `copy` when the primary is
/// the same format (`EPUB copy`).
fn version_label(primary: &Path, copy: &Path, facts: &HashMap<PathBuf, FoldFacts>) -> String {
    let format = format_for_path(copy).unwrap_or(BookFormat::Unreadable);
    // The fold read the file's real verdict (a Kindle file converts or not);
    // the extension alone says only that it is a Kindle file.
    let rank = facts.get(copy).map(|f| f.rank);
    let format = match rank {
        Some(FormatRank::Kindle) => BookFormat::Epub,
        _ => format,
    };
    let name = format_name(copy, format);
    if rank.is_some() && rank == facts.get(primary).map(|f| f.rank) {
        format!("{name} copy")
    } else {
        name
    }
}

/// Count and log each decision (V1391). Query:
/// `sum by (outcome, reason) (increase(pharos_book_fold_total[1d]))`, and
/// `{namespace="pharos"} |= "book formats"` for the files.
pub(crate) fn record(decisions: &[Decision], facts: &HashMap<PathBuf, FoldFacts>) {
    for d in decisions {
        metrics::counter!(
            "pharos_book_fold_total",
            "outcome" => d.outcome.label(),
            "reason" => d.reason.label(),
        )
        .increment(1);
        let author = |p: &Path| {
            facts
                .get(p)
                .and_then(|f| f.author.clone())
                .unwrap_or_default()
        };
        let title = fold_key(&d.copy).map(|(_, t)| t).unwrap_or_default();
        tracing::info!(
            outcome = d.outcome.label(),
            reason = d.reason.label(),
            title = %title,
            primary = %d.primary.display(),
            copy = %d.copy.display(),
            primary_author = %author(&d.primary),
            copy_author = %author(&d.copy),
            "book formats: decided whether a file is a version of another",
        );
    }
}

/// Fold the books of `groups` that are one book: read the facts of each book
/// that shares a title with another (off the reactor, V5), fold, and record
/// each decision.
pub(crate) async fn fold_formats(groups: Vec<Group>) -> Vec<Group> {
    let paths = candidates(&groups);
    if paths.is_empty() {
        return groups;
    }
    let read = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .filter_map(|p| Some((p.clone(), read_fold_facts(&p)?)))
            .collect::<HashMap<_, _>>()
    })
    .await;
    let facts = match read {
        Ok(facts) => facts,
        Err(err) => {
            tracing::warn!(error = %err, "book formats: reading fold facts failed; folding nothing");
            return groups;
        }
    };
    let (groups, decisions) = fold(groups, &facts);
    record(&decisions, &facts);
    groups
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn facts(rank: FormatRank, author: Option<&str>) -> FoldFacts {
        FoldFacts {
            rank,
            author: author.map(str::to_string),
            size: Some(100),
        }
    }

    fn lone(paths: &[&str]) -> Vec<Group> {
        paths
            .iter()
            .map(|p| (PathBuf::from(p), Vec::new()))
            .collect()
    }

    fn table(rows: &[(&str, FoldFacts)]) -> HashMap<PathBuf, FoldFacts> {
        rows.iter()
            .map(|(p, f)| (PathBuf::from(p), f.clone()))
            .collect()
    }

    #[test]
    fn formats_rank_epub_pdf_comic_kindle_locked() {
        let p = Path::new;
        let mut ranks = vec![
            FormatRank::of(p("a.mobi"), BookFormat::Unreadable),
            FormatRank::of(p("a.azw3"), BookFormat::Epub),
            FormatRank::of(p("a.cbz"), BookFormat::Comic),
            FormatRank::of(p("a.pdf"), BookFormat::Pdf),
            FormatRank::of(p("a.epub"), BookFormat::Epub),
        ];
        ranks.sort();
        assert_eq!(
            ranks,
            [
                FormatRank::Epub,
                FormatRank::Pdf,
                FormatRank::Comic,
                FormatRank::Kindle,
                FormatRank::Locked,
            ]
        );
    }

    #[test]
    fn format_names_say_what_the_reader_gets() {
        let p = Path::new;
        assert_eq!(format_name(p("a.epub"), BookFormat::Epub), "EPUB");
        assert_eq!(format_name(p("a.pdf"), BookFormat::Pdf), "PDF");
        assert_eq!(format_name(p("a.cbr"), BookFormat::Comic), "CBR");
        assert_eq!(format_name(p("a.mobi"), BookFormat::Epub), "Kindle");
        assert_eq!(
            format_name(p("a.azw3"), BookFormat::Unreadable),
            "Kindle (locked)"
        );
    }

    #[test]
    fn a_converting_kindle_file_advertises_epub() {
        let p = Path::new;
        assert_eq!(readable_container(p("a.mobi"), BookFormat::Epub), "epub");
        assert_eq!(
            readable_container(p("a.MOBI"), BookFormat::Unreadable),
            "mobi"
        );
        assert_eq!(readable_container(p("a.cbz"), BookFormat::Comic), "cbz");
        for container in ["epub", "pdf", "cbz", "mobi"] {
            assert!(super::super::format_for_extension(container).is_some());
        }
        assert_eq!(
            super::super::format_for_extension("epub"),
            Some(BookFormat::Epub)
        );
    }

    #[test]
    fn the_formats_of_one_book_fold_under_the_best() {
        let groups = lone(&[
            "/b/Widget Cookbook.mobi",
            "/b/Widget Cookbook.pdf",
            "/b/Widget Cookbook.epub",
        ]);
        let f = table(&[
            (
                "/b/Widget Cookbook.mobi",
                facts(FormatRank::Kindle, Some("Ada Field")),
            ),
            (
                "/b/Widget Cookbook.pdf",
                facts(FormatRank::Pdf, Some("Field, Ada")),
            ),
            (
                "/b/Widget Cookbook.epub",
                facts(FormatRank::Epub, Some("Ada Field")),
            ),
        ]);
        let (out, decisions) = fold(groups, &f);
        assert_eq!(
            out,
            vec![(
                PathBuf::from("/b/Widget Cookbook.epub"),
                vec![
                    (
                        "Kindle".to_string(),
                        PathBuf::from("/b/Widget Cookbook.mobi")
                    ),
                    ("PDF".to_string(), PathBuf::from("/b/Widget Cookbook.pdf")),
                ],
            )]
        );
        assert_eq!(decisions.len(), 2);
        assert!(decisions
            .iter()
            .all(|d| d.outcome == FoldOutcome::Folded && d.reason == FoldReason::SameAuthor));
    }

    #[test]
    fn a_numbered_copy_folds_into_the_clean_name() {
        let groups = lone(&["/b/Widget Cookbook (1).epub", "/b/Widget Cookbook.epub"]);
        let f = table(&[
            ("/b/Widget Cookbook (1).epub", facts(FormatRank::Epub, None)),
            (
                "/b/Widget Cookbook.epub",
                facts(FormatRank::Epub, Some("Ada Field")),
            ),
        ]);
        let (out, decisions) = fold(groups, &f);
        assert_eq!(
            out,
            vec![(
                PathBuf::from("/b/Widget Cookbook.epub"),
                vec![(
                    "EPUB copy".to_string(),
                    PathBuf::from("/b/Widget Cookbook (1).epub")
                )],
            )]
        );
        assert_eq!(decisions[0].reason, FoldReason::AuthorUnknown);
    }

    #[test]
    fn titles_match_after_cleaning_and_case_folding() {
        let groups = lone(&["/b/widget_cookbook - Copy.pdf", "/b/Widget Cookbook.epub"]);
        let f = table(&[
            (
                "/b/widget_cookbook - Copy.pdf",
                facts(FormatRank::Pdf, None),
            ),
            ("/b/Widget Cookbook.epub", facts(FormatRank::Epub, None)),
        ]);
        let (out, _) = fold(groups, &f);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, PathBuf::from("/b/Widget Cookbook.epub"));
    }

    #[test]
    fn different_titles_never_fold() {
        let groups = lone(&["/b/Widget Cookbook.pdf", "/b/Widget Cookbook Two.epub"]);
        assert!(candidates(&groups).is_empty());
        let f = table(&[
            ("/b/Widget Cookbook.pdf", facts(FormatRank::Pdf, None)),
            ("/b/Widget Cookbook Two.epub", facts(FormatRank::Epub, None)),
        ]);
        let (out, decisions) = fold(groups.clone(), &f);
        assert_eq!(out, groups);
        assert!(decisions.is_empty());
    }

    #[test]
    fn different_authors_keep_two_books_apart() {
        let groups = lone(&["/b/Gardening.epub", "/b/Gardening.pdf"]);
        let f = table(&[
            (
                "/b/Gardening.epub",
                facts(FormatRank::Epub, Some("Ada Field")),
            ),
            (
                "/b/Gardening.pdf",
                facts(FormatRank::Pdf, Some("Ben Stone")),
            ),
        ]);
        let (out, decisions) = fold(groups.clone(), &f);
        assert_eq!(out, groups);
        assert_eq!(
            decisions,
            vec![Decision {
                primary: PathBuf::from("/b/Gardening.epub"),
                copy: PathBuf::from("/b/Gardening.pdf"),
                outcome: FoldOutcome::KeptApart,
                reason: FoldReason::AuthorsDiffer,
            }]
        );
    }

    /// A file another author's book kept apart still folds with its own
    /// author's other format.
    #[test]
    fn a_kept_apart_book_folds_with_its_own_formats() {
        let groups = lone(&["/b/Gardening.epub", "/b/Gardening.pdf", "/b/Gardening.mobi"]);
        let f = table(&[
            (
                "/b/Gardening.epub",
                facts(FormatRank::Epub, Some("Ada Field")),
            ),
            (
                "/b/Gardening.pdf",
                facts(FormatRank::Pdf, Some("Ben Stone")),
            ),
            (
                "/b/Gardening.mobi",
                facts(FormatRank::Locked, Some("Ben Stone")),
            ),
        ]);
        let (out, _) = fold(groups, &f);
        assert_eq!(
            out,
            vec![
                (PathBuf::from("/b/Gardening.epub"), Vec::new()),
                (
                    PathBuf::from("/b/Gardening.pdf"),
                    vec![(
                        "Kindle (locked)".to_string(),
                        PathBuf::from("/b/Gardening.mobi")
                    )],
                ),
            ]
        );
    }

    #[test]
    fn different_folders_never_fold() {
        let groups = lone(&["/b/one/Widget Cookbook.epub", "/b/two/Widget Cookbook.pdf"]);
        assert!(candidates(&groups).is_empty());
        let f = table(&[
            ("/b/one/Widget Cookbook.epub", facts(FormatRank::Epub, None)),
            ("/b/two/Widget Cookbook.pdf", facts(FormatRank::Pdf, None)),
        ]);
        let (out, _) = fold(groups.clone(), &f);
        assert_eq!(out, groups);
    }

    #[test]
    fn a_video_of_the_same_name_is_no_book_format() {
        let groups = lone(&["/b/Widget Cookbook.epub", "/b/Widget Cookbook.mkv"]);
        assert!(candidates(&groups).is_empty());
    }

    #[test]
    fn authors_compare_without_case_punctuation_or_order() {
        assert_eq!(
            FoldReason::of(Some("Field, Ada"), Some("ada field")),
            FoldReason::SameAuthor
        );
        assert_eq!(
            FoldReason::of(Some("Ada Field"), None),
            FoldReason::AuthorUnknown
        );
        assert_eq!(
            FoldReason::of(Some(" "), Some("Ada")),
            FoldReason::AuthorUnknown
        );
        assert_eq!(
            FoldReason::of(Some("Ada Field"), Some("Ben Stone")),
            FoldReason::AuthorsDiffer
        );
    }

    #[test]
    fn fold_labels_are_distinct_and_stable() {
        let outcomes: std::collections::HashSet<_> =
            FoldOutcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(outcomes.len(), FoldOutcome::ALL.len());
        let reasons: std::collections::HashSet<_> =
            FoldReason::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(reasons.len(), FoldReason::ALL.len());
        assert_eq!(FoldOutcome::Folded.label(), "folded");
        assert_eq!(FoldOutcome::KeptApart.label(), "kept_apart");
        assert_eq!(FoldReason::AuthorsDiffer.label(), "authors_differ");
    }

    fn fold_count(
        snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
        outcome: &str,
        reason: &str,
    ) -> u64 {
        use metrics_util::debugging::DebugValue;
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_book_fold_total")
            .filter(|(k, ..)| {
                let labels: Vec<_> = k.key().labels().map(|l| (l.key(), l.value())).collect();
                labels.contains(&("outcome", outcome)) && labels.contains(&("reason", reason))
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(n) => n,
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn each_decision_is_counted_with_its_outcome_and_reason() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let groups = lone(&[
            "/b/Gardening.epub",
            "/b/Gardening.pdf",
            "/b/Gardening (1).epub",
        ]);
        let f = table(&[
            (
                "/b/Gardening.epub",
                facts(FormatRank::Epub, Some("Ada Field")),
            ),
            (
                "/b/Gardening.pdf",
                facts(FormatRank::Pdf, Some("Ben Stone")),
            ),
            ("/b/Gardening (1).epub", facts(FormatRank::Epub, None)),
        ]);
        let (_, decisions) = fold(groups, &f);
        record(&decisions, &f);
        assert_eq!(fold_count(&snapshotter, "folded", "author_unknown"), 1);
        assert_eq!(fold_count(&snapshotter, "kept_apart", "authors_differ"), 1);
        assert_eq!(fold_count(&snapshotter, "folded", "same_author"), 0);
    }
}
