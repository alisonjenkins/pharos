//! Spec 037 — a film split across files (`cd1`, `cd2`, `Part 2` after the
//! year) is one film: its first part is the item and the rest ride on it as
//! numbered parts (V1150). Files are stacked only when their names agree but
//! for the part number, the numbers run 1..=n, and no two parts' embedded
//! titles name different films (V1151).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use pharos_core::MediaStore;

/// A primary file and its `(label, path)` alternates, as the scan groups them.
pub(crate) type Group = (PathBuf, Vec<(String, PathBuf)>);

/// The label a stacked part carries among its film's alternates. It holds a
/// control character so no edition tag read from a file name can match it.
const PART_LABEL_PREFIX: &str = "\u{1}part:";

/// The containers a split film's parts are held in.
const PART_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "mov", "avi", "webm", "m4v", "mpg", "mpeg", "ts", "m2ts", "wmv",
];

/// The alternate label for part `n` of a film.
pub(crate) fn part_label(n: u32) -> String {
    format!("{PART_LABEL_PREFIX}{n}")
}

/// The part number an alternate label names, if it names one.
pub(crate) fn label_part(label: &str) -> Option<u32> {
    label.strip_prefix(PART_LABEL_PREFIX)?.parse().ok()
}

/// Whether `group` is a film stacked from its parts.
pub(crate) fn is_stack(group: &Group) -> bool {
    group.1.iter().any(|(label, _)| label_part(label).is_some())
}

/// A part marker: `cd2`, `CD 2`, `disc-2`, `pt2` or `part 2`, standing alone
/// as a word. The last one in a name is its part number.
static PART_RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)(?:^|[\s._\-\[\(])(?P<word>cd|disc|disk|part|pt)[\s._-]?0?(?P<n>[1-9][0-9]?)(?:$|[\s._\-\]\)])",
    )
    .ok()
});

static YEAR_RE: std::sync::LazyLock<Option<regex::Regex>> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\b(?:19|20)[0-9]{2}\b").ok());

/// The words a name is compared by: lower-cased, split on anything that is
/// not a letter or digit.
fn folded(name: &str) -> String {
    name.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A film file's part number and the rest of its name, folded: `Heat (1995)
/// cd2.mkv` is `("heat 1995", 2)`. `part N` and `pt N` count only after a
/// year, since before one they are the title (`Part 1 (2010)` and `Part 2
/// (2011)` are two films). An episode, a song or a book is never a part.
pub(crate) fn part_of(path: &Path) -> Option<(String, u32)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !PART_EXTENSIONS.contains(&ext.as_str()) || crate::fs::is_episode_file(path) {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let caps = PART_RE.as_ref()?.captures_iter(stem).last()?;
    let (all, word, n) = (caps.get(0)?, caps.name("word")?, caps.name("n")?);
    let titled = matches!(word.as_str().to_ascii_lowercase().as_str(), "part" | "pt");
    if titled {
        let before = stem.get(..all.start())?;
        if !YEAR_RE.as_ref()?.is_match(before) {
            return None;
        }
    }
    let rest = format!("{} {}", stem.get(..all.start())?, stem.get(all.end()..)?);
    Some((folded(&rest), n.as_str().parse().ok()?))
}

/// An embedded title less any part marker in it, folded, so `Heat CD1` and
/// `Heat CD2` agree and `Heat` and `Heat 2` do not.
fn title_key(title: &str) -> String {
    let stripped = match PART_RE.as_ref() {
        Some(re) => re.replace_all(title, " ").into_owned(),
        None => title.to_string(),
    };
    folded(&stripped)
}

/// Why files whose names make them parts of one film stay apart. The
/// `label()` strings are the `outcome` values of
/// `pharos_scan_part_stack_total`, a dashboard contract: do not rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StackOutcome {
    /// The parts became one film.
    Stacked,
    /// The numbers do not run 1..=n (a gap, a repeat, no part 1).
    Numbering,
    /// A part already has versions of its own.
    Versions,
    /// A part has never been probed, so its embedded title is unknown; the
    /// next scan, after this one indexes it, decides.
    Unprobed,
    /// Two parts' embedded titles name different films.
    TitlesDiffer,
    /// A part's probe found no duration, so its film's playlist could not
    /// run on past it.
    NoDuration,
}

impl StackOutcome {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 6] = [
        Self::Stacked,
        Self::Numbering,
        Self::Versions,
        Self::Unprobed,
        Self::TitlesDiffer,
        Self::NoDuration,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Stacked => "stacked",
            Self::Numbering => "numbering",
            Self::Versions => "versions",
            Self::Unprobed => "unprobed",
            Self::TitlesDiffer => "titles_differ",
            Self::NoDuration => "no_duration",
        }
    }
}

/// Files in one folder whose names agree but for their part numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// Each group's index and its part number, in part order.
    pub(crate) parts: Vec<(usize, u32)>,
}

/// The sets of `groups` that may be one film's parts: two or more files in
/// one folder whose names fold alike once their part markers are dropped.
pub(crate) fn candidates(groups: &[Group]) -> Vec<Candidate> {
    let mut by_name: BTreeMap<(PathBuf, String), Vec<(usize, u32)>> = BTreeMap::new();
    for (i, (path, _)) in groups.iter().enumerate() {
        let Some((rest, n)) = part_of(path) else {
            continue;
        };
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        by_name.entry((dir, rest)).or_default().push((i, n));
    }
    by_name
        .into_values()
        .filter(|parts| parts.len() > 1)
        .map(|mut parts| {
            parts.sort_by_key(|&(i, n)| (n, i));
            Candidate { parts }
        })
        .collect()
}

/// What the store says a probed file holds. A file with no entry has never
/// been probed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StoredPart {
    pub(crate) title: Option<String>,
    pub(crate) duration_ms: Option<u64>,
}

/// Each candidate file's stored title and duration: from its own row, or from the
/// film it was stacked into on an earlier scan. A store fault leaves the file
/// unprobed, so the parts stay apart this scan rather than merge blind.
pub(crate) async fn stored_titles<S: MediaStore>(
    store: &S,
    groups: &[Group],
    candidates: &[Candidate],
) -> HashMap<PathBuf, StoredPart> {
    let mut out: HashMap<PathBuf, StoredPart> = HashMap::new();
    for candidate in candidates {
        for &(i, _) in &candidate.parts {
            let Some((path, _)) = groups.get(i) else {
                continue;
            };
            if out.contains_key(path) {
                continue;
            }
            let row = match store.id_at_path(path).await {
                Ok(Some(row)) => row,
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "could not look up a part's row to read its title");
                    continue;
                }
            };
            let item = match store.get(row).await {
                Ok(item) => item,
                Err(err) => {
                    tracing::warn!(path = %path.display(), row = %row, error = %err, "could not read a part's row to read its title");
                    continue;
                }
            };
            for alt in &item.probe.alternate_sources {
                if let Some(probe) = alt.probe.as_ref() {
                    out.entry(alt.path.clone()).or_insert_with(|| StoredPart {
                        title: probe.title.clone(),
                        duration_ms: probe.duration_ms.or(alt.duration_ms),
                    });
                }
            }
            out.insert(
                item.path.clone(),
                StoredPart {
                    title: item.probe.title.clone(),
                    duration_ms: item.probe.duration_ms,
                },
            );
        }
    }
    out
}

/// Whether `candidate`'s files are one film's parts, by their numbers, their
/// versions, their stored embedded titles and their durations. On a refusal,
/// the value that decided it.
pub(crate) fn verdict(
    groups: &[Group],
    candidate: &Candidate,
    titles: &HashMap<PathBuf, StoredPart>,
) -> (StackOutcome, String) {
    let numbers: Vec<u32> = candidate.parts.iter().map(|&(_, n)| n).collect();
    if numbers.iter().zip(1..).any(|(&n, want)| n != want) {
        return (StackOutcome::Numbering, format!("{numbers:?}"));
    }
    let path_of = |i: usize| groups.get(i).map(|g| g.0.as_path());
    if let Some(i) = candidate
        .parts
        .iter()
        .map(|&(i, _)| i)
        .find(|&i| groups.get(i).is_some_and(|g| !g.1.is_empty()))
    {
        return (
            StackOutcome::Versions,
            path_of(i)
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
    }
    let mut seen: Option<(String, String)> = None;
    for &(i, _) in &candidate.parts {
        let Some(path) = path_of(i) else {
            continue;
        };
        let Some(stored) = titles.get(path) else {
            return (StackOutcome::Unprobed, path.display().to_string());
        };
        if stored.duration_ms.unwrap_or(0) == 0 {
            return (StackOutcome::NoDuration, path.display().to_string());
        }
        let Some(title) = stored.title.as_deref().filter(|t| !title_key(t).is_empty()) else {
            continue;
        };
        let key = title_key(title);
        match &seen {
            Some((first, raw)) if *first != key => {
                return (StackOutcome::TitlesDiffer, format!("{raw:?} vs {title:?}"));
            }
            Some(_) => {}
            None => seen = Some((key, title.to_string())),
        }
    }
    (StackOutcome::Stacked, String::new())
}

/// Count one stacking decision and log it with the files and the value that
/// decided it: a refusal at info, a stack at debug.
pub(crate) fn record(groups: &[Group], candidate: &Candidate, outcome: StackOutcome, value: &str) {
    let files: Vec<String> = candidate
        .parts
        .iter()
        .filter_map(|&(i, _)| groups.get(i).map(|g| g.0.display().to_string()))
        .collect();
    metrics::counter!("pharos_scan_part_stack_total", "outcome" => outcome.label()).increment(1);
    if outcome == StackOutcome::Stacked {
        tracing::debug!(parts = files.len(), files = ?files, "stacked a film's parts");
    } else {
        tracing::info!(
            outcome = outcome.label(),
            value = %value,
            files = ?files,
            "left a film's parts apart",
        );
    }
}

/// Stack each candidate that passes [`verdict`]: its first part keeps its
/// group and the others join it as numbered parts. Every decision is logged
/// with the files and the value that decided it, and counted.
pub(crate) fn stack(
    groups: Vec<Group>,
    candidates: &[Candidate],
    titles: &HashMap<PathBuf, StoredPart>,
) -> Vec<Group> {
    let mut joins: HashMap<usize, Vec<(u32, usize)>> = HashMap::new();
    let mut taken = vec![false; groups.len()];
    for candidate in candidates {
        let (outcome, value) = verdict(&groups, candidate, titles);
        record(&groups, candidate, outcome, &value);
        if outcome != StackOutcome::Stacked {
            continue;
        }
        let Some(&(first, _)) = candidate.parts.first() else {
            continue;
        };
        for &(i, n) in candidate.parts.iter().skip(1) {
            if let Some(flag) = taken.get_mut(i) {
                *flag = true;
            }
            joins.entry(first).or_default().push((n, i));
        }
    }
    let paths: Vec<PathBuf> = groups.iter().map(|g| g.0.clone()).collect();
    groups
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !taken.get(*i).copied().unwrap_or(false))
        .map(|(i, (primary, mut alts))| {
            for &(n, part) in joins.get(&i).map(Vec::as_slice).unwrap_or_default() {
                if let Some(path) = paths.get(part) {
                    alts.push((part_label(n), path.clone()));
                }
            }
            (primary, alts)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn group(path: &str) -> Group {
        (PathBuf::from(path), Vec::new())
    }

    fn titles(entries: &[(&str, Option<&str>)]) -> HashMap<PathBuf, StoredPart> {
        entries
            .iter()
            .map(|(p, t)| {
                (
                    PathBuf::from(p),
                    StoredPart {
                        title: t.map(str::to_string),
                        duration_ms: Some(60_000),
                    },
                )
            })
            .collect()
    }

    /// V1150 — a part marker after the year, or a disc marker anywhere, names
    /// a part; `Part N` before a year is a title.
    #[test]
    fn a_name_states_its_part() {
        let part = |p: &str| part_of(Path::new(p));
        assert_eq!(
            part("/m/Glass Harbour (2003)/Glass Harbour (2003) cd2.mp4"),
            Some(("glass harbour 2003".into(), 2))
        );
        assert_eq!(
            part("/m/G/Glass.Harbour.2003.DVDRip.CD1.avi"),
            Some(("glass harbour 2003 dvdrip".into(), 1))
        );
        assert_eq!(
            part("/m/G/Glass Harbour (2003) Part 2 Bluray-1080p.mkv"),
            Some(("glass harbour 2003 bluray 1080p".into(), 2))
        );
        assert_eq!(
            part("/m/G/Glass Harbour Disc 2.mkv"),
            Some(("glass harbour".into(), 2))
        );
        assert_eq!(part("/m/G/Glass Harbour Part 1 (2010).mkv"), None);
        assert_eq!(part("/m/G/Glass Harbour Part 2.mkv"), None);
        assert_eq!(part("/m/G/Glass Harbour (2003).mkv"), None);
        assert_eq!(part("/m/G/Glass Harbour (2003) cd2.srt"), None);
        assert_eq!(part("/m/Show/Season 01/Show S01E02 cd2.mkv"), None);
    }

    /// V1150 — parts in one folder whose names agree become one film, its
    /// first part the item and the rest its numbered parts, whatever their
    /// containers.
    #[test]
    fn a_films_parts_become_one_film() {
        let groups = vec![
            group("/m/G/Glass Harbour (2003) cd2.mp4"),
            group("/m/G/Glass Harbour (2003) cd1.mkv"),
            group("/m/G/poster-less.mkv"),
        ];
        let found = candidates(&groups);
        let titles = titles(&[
            (
                "/m/G/Glass Harbour (2003) cd1.mkv",
                Some("Glass Harbour CD1"),
            ),
            ("/m/G/Glass Harbour (2003) cd2.mp4", None),
        ]);
        let out = stack(groups, &found, &titles);
        assert_eq!(out.len(), 2);
        let film = out.iter().find(|g| is_stack(g)).unwrap();
        assert_eq!(film.0, PathBuf::from("/m/G/Glass Harbour (2003) cd1.mkv"));
        assert_eq!(
            film.1,
            vec![(
                part_label(2),
                PathBuf::from("/m/G/Glass Harbour (2003) cd2.mp4")
            )]
        );
        assert_eq!(label_part(&film.1[0].0), Some(2));
    }

    /// V1151 — files named as parts whose embedded titles name different
    /// films stay separate films.
    #[test]
    fn parts_whose_titles_name_different_films_stay_apart() {
        let groups = vec![
            group("/m/S/Saga (1990) cd1.avi"),
            group("/m/S/Saga (1990) cd2.avi"),
            group("/m/S/Saga (1990) cd3.avi"),
        ];
        let found = candidates(&groups);
        let titles = titles(&[
            ("/m/S/Saga (1990) cd1.avi", Some("Saga")),
            ("/m/S/Saga (1990) cd2.avi", Some("Saga II: The Return")),
            ("/m/S/Saga (1990) cd3.avi", Some("Saga III")),
        ]);
        let (outcome, value) = verdict(&groups, &found[0], &titles);
        assert_eq!(outcome, StackOutcome::TitlesDiffer);
        assert!(value.contains("Saga II"), "{value}");
        assert_eq!(stack(groups, &found, &titles).len(), 3);
    }

    /// V1151 — a gap in the numbers, a part never probed, a part with
    /// versions of its own, or a part with no duration keeps the files apart.
    #[test]
    fn doubtful_parts_stay_apart() {
        let gap = vec![
            group("/m/G/G (2003) cd1.mkv"),
            group("/m/G/G (2003) cd3.mkv"),
        ];
        let all = titles(&[
            ("/m/G/G (2003) cd1.mkv", None),
            ("/m/G/G (2003) cd3.mkv", None),
        ]);
        assert_eq!(
            verdict(&gap, &candidates(&gap)[0], &all).0,
            StackOutcome::Numbering
        );

        let pair = vec![
            group("/m/G/G (2003) cd1.mkv"),
            group("/m/G/G (2003) cd2.mkv"),
        ];
        let one = titles(&[("/m/G/G (2003) cd1.mkv", None)]);
        let (outcome, value) = verdict(&pair, &candidates(&pair)[0], &one);
        assert_eq!(outcome, StackOutcome::Unprobed);
        assert!(value.ends_with("cd2.mkv"), "{value}");

        let mut versioned = pair.clone();
        versioned[1].1.push((
            "Extended".into(),
            PathBuf::from("/m/G/G (2003) cd2 - Extended.mkv"),
        ));
        let both = titles(&[
            ("/m/G/G (2003) cd1.mkv", None),
            ("/m/G/G (2003) cd2.mkv", None),
        ]);
        assert_eq!(
            verdict(&versioned, &candidates(&versioned)[0], &both).0,
            StackOutcome::Versions
        );

        let mut timeless = both.clone();
        if let Some(cd2) = timeless.get_mut(Path::new("/m/G/G (2003) cd2.mkv")) {
            cd2.duration_ms = None;
        }
        let mut zero = both.clone();
        if let Some(cd2) = zero.get_mut(Path::new("/m/G/G (2003) cd2.mkv")) {
            cd2.duration_ms = Some(0);
        }
        assert_eq!(
            verdict(&pair, &candidates(&pair)[0], &zero).0,
            StackOutcome::NoDuration
        );
        let found = candidates(&pair);
        let (outcome, value) = verdict(&pair, &found[0], &timeless);
        assert_eq!(outcome, StackOutcome::NoDuration);
        assert!(value.ends_with("cd2.mkv"), "{value}");
        assert_eq!(stack(pair, &found, &timeless).len(), 2);
    }

    #[test]
    fn stack_outcome_labels_are_distinct() {
        let labels: std::collections::HashSet<_> =
            StackOutcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), StackOutcome::ALL.len());
    }
}
