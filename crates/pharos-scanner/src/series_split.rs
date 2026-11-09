//! Spec 033 — which show each episode file belongs to, decided per folder.
//!
//! A file states a show year (V890): its file name's `Show (YYYY)` prefix when
//! the name is the folder's show, else the folder's `(YYYY)`, else its release
//! folder's. A folder whose files all state one year takes it as the show's
//! year. A folder whose files state two or more years holds two shows and is
//! split by year (V893): the group that keeps the plain folder key is fixed
//! once and remembered as a `scanner` override; every other group gets
//! `"{folder}\u{1f}{YYYY}"`.
//!
//! [`plan`] is pure; [`reconcile`] diffs the plan with the stored rows and
//! writes (or, in dry run, only logs) the differences.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use pharos_core::{
    DomainResult, OverrideSource, SeriesIdentityRow, SeriesIdentityStore, SeriesKeyUpdate,
    SeriesOverride,
};

use crate::fs::{name_show_name_and_year, parse_series_info};

/// Separates a folder from the year in a split group's key. A unit separator
/// never occurs in a path.
pub const SPLIT_KEY_SEPARATOR: char = '\u{1f}';

/// Lowest [`pharos_core::title_similarity`] at which a file name's show name
/// is the folder's show (the bar B558 checks an NFO's show name by).
const SAME_SHOW_NAME: f32 = 0.8;

/// Whether the scanner may write what it decides about series identity.
/// `[metadata] series_identity_repair` (spec 033 §6.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairMode {
    /// Log and count what would change; write nothing.
    #[default]
    DryRun,
    /// Write it.
    Apply,
}

impl RepairMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Apply => "apply",
        }
    }
}

/// The key a group of `folder` with stated year `year` gets when it does not
/// keep the plain folder key.
pub fn split_key(folder: &str, year: u32) -> String {
    format!("{folder}{SPLIT_KEY_SEPARATOR}{year}")
}

/// The show year an episode file states (V890), and where it came from.
pub fn stated_show_year(path: &Path) -> Option<u32> {
    let info = parse_series_info(path)?;
    let name = path.file_name()?.to_str()?;
    let from_name = name_show_name_and_year(name).and_then(|(show, year)| {
        (pharos_core::title_similarity(show, &info.series_name) >= SAME_SHOW_NAME).then_some(year)
    });
    from_name.or(info.series_year)
}

/// What one file's row should hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// `Some(folder)` for the group that keeps the plain key, else the split key.
    pub series_key: String,
    pub series_year: Option<u32>,
}

/// One folder whose files state two or more years.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub folder: String,
    /// Files per stated year (`None` = no year stated).
    pub groups: BTreeMap<Option<u32>, usize>,
    /// The year whose group keeps the plain folder key.
    pub owner_year: u32,
    /// How the owner was chosen: `override`, `stored_match` or `largest`.
    pub owner_by: &'static str,
}

impl Split {
    /// `none:4, 2014:62`, for the log line.
    pub fn groups_label(&self) -> String {
        self.groups
            .iter()
            .map(|(y, n)| match y {
                Some(y) => format!("{y}:{n}"),
                None => format!("none:{n}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The split plan for one walk.
#[derive(Debug, Default)]
pub struct SeriesPlan {
    /// Every episode folder the walk saw; rows of other folders are not
    /// this pass's to change.
    pub walked: std::collections::BTreeSet<String>,
    pub assignments: HashMap<PathBuf, Assignment>,
    pub splits: Vec<Split>,
    /// Owner rows to remember, for folders split for the first time.
    pub new_owners: Vec<SeriesOverride>,
}

/// What the store already knows about the folders in a walk.
#[derive(Debug, Default)]
pub struct PlanContext {
    pub overrides: Vec<SeriesOverride>,
    /// Premiere year of each folder's stored show match, by series key.
    pub premiere_years: HashMap<String, u32>,
}

/// Each episode file's folder and stated year, grouped by folder.
pub fn folders_of(paths: &[PathBuf]) -> BTreeMap<String, Vec<(PathBuf, Option<u32>)>> {
    let mut folders: BTreeMap<String, Vec<(PathBuf, Option<u32>)>> = BTreeMap::new();
    for path in paths {
        if !crate::fs::is_episode_path(path) {
            continue;
        }
        let Some(folder) = parse_series_info(path).and_then(|i| i.series_folder) else {
            continue;
        };
        folders
            .entry(folder)
            .or_default()
            .push((path.clone(), stated_show_year(path)));
    }
    folders
}

/// The folders whose files state two or more years; the only ones whose
/// stored match the plan needs.
pub fn split_candidates(folders: &BTreeMap<String, Vec<(PathBuf, Option<u32>)>>) -> Vec<String> {
    folders
        .iter()
        .filter(|(_, files)| distinct_years(files).len() > 1)
        .map(|(f, _)| f.clone())
        .collect()
}

fn distinct_years(files: &[(PathBuf, Option<u32>)]) -> std::collections::BTreeSet<u32> {
    files.iter().filter_map(|(_, y)| *y).collect()
}

/// Decide every episode file's key and year. `now` stamps new owner rows.
pub fn plan(
    folders: &BTreeMap<String, Vec<(PathBuf, Option<u32>)>>,
    ctx: &PlanContext,
    now: i64,
) -> SeriesPlan {
    let mut out = SeriesPlan::default();
    for (folder, files) in folders {
        out.walked.insert(folder.clone());
        let years = distinct_years(files);
        let mut assign = |path: &PathBuf, key: String, year: Option<u32>| {
            out.assignments.insert(
                path.clone(),
                Assignment {
                    series_key: key,
                    series_year: year,
                },
            );
        };
        match years.len() {
            0 => {}
            // One stated year: the folder's show is that year's; files that
            // state none are the same show (V890 consensus).
            1 => {
                let year = years.iter().next().copied();
                for (path, _) in files {
                    assign(path, folder.clone(), year);
                }
            }
            _ => {
                let mut groups: BTreeMap<Option<u32>, usize> = BTreeMap::new();
                for (_, y) in files {
                    *groups.entry(*y).or_default() += 1;
                }
                let (owner_year, owner_by) = owner(folder, &groups, ctx);
                for (path, year) in files {
                    let year = year.unwrap_or(owner_year);
                    let key = if year == owner_year {
                        folder.clone()
                    } else {
                        split_key(folder, year)
                    };
                    assign(path, key, Some(year));
                }
                if owner_by != "override" {
                    out.new_owners.push(SeriesOverride {
                        id: 0,
                        series_folder: folder.clone(),
                        path: None,
                        target_key: folder.clone(),
                        target_name: None,
                        target_year: Some(owner_year),
                        source: OverrideSource::Scanner,
                        created_by: None,
                        created_at: now,
                    });
                }
                out.splits.push(Split {
                    folder: folder.clone(),
                    groups,
                    owner_year,
                    owner_by,
                });
            }
        }
        place_by_override(folder, files, ctx, &mut out.assignments);
    }
    out
}

/// A file a `series_overrides` row names goes where the row says, whatever
/// its stated year; an `admin` row wins over a `scanner` one.
fn place_by_override(
    folder: &str,
    files: &[(PathBuf, Option<u32>)],
    ctx: &PlanContext,
    assignments: &mut HashMap<PathBuf, Assignment>,
) {
    let mut rows: Vec<&SeriesOverride> = ctx
        .overrides
        .iter()
        .filter(|o| o.series_folder == folder && o.path.is_some())
        .collect();
    // Scanner rows first, so an admin row for the same path lands last.
    rows.sort_by_key(|o| o.source == OverrideSource::Admin);
    for o in rows {
        let Some(path) = o.path.as_deref().map(Path::new) else {
            continue;
        };
        if let Some((path, _)) = files.iter().find(|(p, _)| p == path) {
            assignments.insert(
                path.clone(),
                Assignment {
                    series_key: o.target_key.clone(),
                    series_year: o.target_year,
                },
            );
        }
    }
}

/// Which stated year's group keeps `folder`'s plain key: a remembered owner
/// row, else the group within a year of the stored match's premiere, else the
/// group with the most files (the earlier year on a tie).
fn owner(
    folder: &str,
    groups: &BTreeMap<Option<u32>, usize>,
    ctx: &PlanContext,
) -> (u32, &'static str) {
    let yeared = || groups.iter().filter_map(|(y, n)| Some(((*y)?, *n)));
    let remembered = ctx
        .overrides
        .iter()
        .filter(|o| o.series_folder == folder && o.path.is_none() && o.target_key == folder)
        .max_by_key(|o| o.source == OverrideSource::Admin)
        .and_then(|o| o.target_year)
        .filter(|y| groups.contains_key(&Some(*y)));
    if let Some(y) = remembered {
        return (y, "override");
    }
    if let Some(premiere) = ctx.premiere_years.get(folder) {
        if let Some((y, _)) = yeared()
            .filter(|(y, _)| y.abs_diff(*premiere) <= 1)
            .min_by_key(|(y, _)| y.abs_diff(*premiere))
        {
            return (y, "stored_match");
        }
    }
    let largest = yeared()
        .max_by(|(ya, na), (yb, nb)| na.cmp(nb).then(yb.cmp(ya)))
        .map_or(0, |(y, _)| y);
    (largest, "largest")
}

/// The rows whose stored key or year differs from `plan`, and the rows of a
/// walked folder a split no longer covers (a split key whose group is gone
/// goes back to the folder, keeping its year).
pub fn diff(plan: &SeriesPlan, stored: &[SeriesIdentityRow]) -> Vec<SeriesKeyUpdate> {
    let mut updates = Vec::new();
    for row in stored {
        let Some(folder) = row.series_folder.as_deref() else {
            continue;
        };
        if !plan.walked.contains(folder) {
            continue;
        }
        let current_key = row.series_key.as_deref().unwrap_or(folder);
        match plan.assignments.get(Path::new(&row.path)) {
            Some(a) => {
                if a.series_key != current_key || a.series_year != row.series_year {
                    updates.push(SeriesKeyUpdate {
                        path: row.path.clone(),
                        series_key: (a.series_key != folder).then(|| a.series_key.clone()),
                        series_year: a.series_year,
                    });
                }
            }
            None if current_key != folder => updates.push(SeriesKeyUpdate {
                path: row.path.clone(),
                series_key: None,
                series_year: row.series_year,
            }),
            None => {}
        }
    }
    updates
}

/// Folders per `series_overrides` read, well under SQLite's bind limit.
const OVERRIDE_READ_CHUNK: usize = 500;

/// Load what [`plan`] needs about `folders` from the store: every folder's
/// overrides, and the stored match of the folders a split may touch.
pub async fn context<S: SeriesIdentityStore>(
    store: &S,
    folders: &BTreeMap<String, Vec<(PathBuf, Option<u32>)>>,
) -> DomainResult<PlanContext> {
    let names: Vec<String> = folders.keys().cloned().collect();
    let mut overrides = Vec::new();
    for chunk in names.chunks(OVERRIDE_READ_CHUNK) {
        overrides.extend(store.series_overrides(chunk).await?);
    }
    let candidates = split_candidates(folders);
    let premiere_years = if candidates.is_empty() {
        HashMap::new()
    } else {
        store.series_premiere_years(&candidates).await?
    };
    Ok(PlanContext {
        overrides,
        premiere_years,
    })
}

/// Write (in [`RepairMode::Apply`]) or log (in dry run) every difference
/// between `plan` and the rows stored under `root_prefix`. Returns the rows
/// changed. A dry run finds the same differences every pass, so its
/// per-row lines are debug and its per-folder line is the info one.
pub async fn reconcile<S: SeriesIdentityStore>(
    store: &S,
    plan: &SeriesPlan,
    root_prefix: &str,
    mode: RepairMode,
) -> DomainResult<u64> {
    for split in &plan.splits {
        tracing::info!(
            folder = %split.folder,
            groups = %format!("[{}]", split.groups_label()),
            owner_year = split.owner_year,
            owner_by = split.owner_by,
            reason = "stated_years",
            mode = mode.label(),
            "series folder split"
        );
    }
    metrics::gauge!(
        "pharos_series_folder_split_total",
        "reason" => "stated_years",
        "root" => root_prefix.to_string(),
    )
    .set(plan.splits.len() as f64);
    let stored = store.series_identity_rows(root_prefix).await?;
    let updates = diff(plan, &stored);
    let by_path: HashMap<&str, &SeriesIdentityRow> =
        stored.iter().map(|r| (r.path.as_str(), r)).collect();
    let mut per_folder: BTreeMap<&str, usize> = BTreeMap::new();
    for u in &updates {
        let old = by_path.get(u.path.as_str());
        if let Some(folder) = old.and_then(|r| r.series_folder.as_deref()) {
            *per_folder.entry(folder).or_default() += 1;
        }
        tracing::debug!(
            path = %u.path,
            old_key = old.and_then(|r| r.series_key.as_deref()).unwrap_or("folder"),
            new_key = u.series_key.as_deref().unwrap_or("folder"),
            old_year = old.and_then(|r| r.series_year),
            new_year = u.series_year,
            mode = mode.label(),
            "series identity repair: row"
        );
    }
    let owners: std::collections::BTreeSet<&str> = plan
        .new_owners
        .iter()
        .map(|o| o.series_folder.as_str())
        .collect();
    for (folder, rows) in &per_folder {
        tracing::info!(
            folder,
            rows,
            split = plan.splits.iter().any(|s| s.folder == *folder),
            new_owner_row = owners.contains(folder),
            mode = mode.label(),
            "series identity repair"
        );
    }
    set_repair_rows("series_key", mode, root_prefix, updates.len());
    set_repair_rows("owner_row", mode, root_prefix, plan.new_owners.len());
    if mode == RepairMode::DryRun {
        return Ok(0);
    }
    for o in &plan.new_owners {
        store.insert_series_override(o.clone()).await?;
    }
    store.set_series_keys(&updates).await
}

/// Rows the last complete pass of `root` found to differ: written in
/// apply (so a steady library reads 0), still pending in dry run.
fn set_repair_rows(change: &'static str, mode: RepairMode, root: &str, n: usize) {
    metrics::gauge!(
        "pharos_series_identity_repair_rows",
        "change" => change,
        "mode" => mode.label(),
        "root" => root.to_string(),
    )
    .set(n as f64);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    fn row(path: &str, folder: &str, key: Option<&str>, year: Option<u32>) -> SeriesIdentityRow {
        SeriesIdentityRow {
            path: path.into(),
            series_folder: Some(folder.into()),
            series_key: key.map(Into::into),
            series_year: year,
        }
    }

    #[test]
    fn a_file_states_its_name_prefix_year_over_its_folders() {
        assert_eq!(
            stated_show_year(Path::new(
                "/tv/Battlestar Galactica/Season 1/Battlestar Galactica (2003) - S01E01 - 33.mkv"
            )),
            Some(2003)
        );
        // Filename ▸ folder: the 2014 file in the 1980 folder states 2014.
        assert_eq!(
            stated_show_year(Path::new(
                "/tv/Cosmos (1980)/Season 1/Cosmos (2014) - S01E01.mkv"
            )),
            Some(2014)
        );
        assert_eq!(
            stated_show_year(Path::new("/tv/Cosmos (1980)/Season 1/Cosmos - S01E01.mkv")),
            Some(1980)
        );
        assert_eq!(
            stated_show_year(Path::new("/tv/The Flash/Season 1/The Flash - S01E01.mkv")),
            None
        );
        // Another show's name prefix says nothing about this folder's show.
        assert_eq!(
            stated_show_year(Path::new(
                "/tv/The Flash/Season 1/Doctor Who (2005) - S01E01.mkv"
            )),
            None
        );
    }

    #[test]
    fn a_folder_whose_files_agree_takes_their_year_without_splitting() {
        let folders = folders_of(&paths(&[
            "/tv/Battlestar Galactica/Season 1/Battlestar Galactica (2003) - S01E01.mkv",
            "/tv/Battlestar Galactica/Season 1/Battlestar Galactica (2003) - S01E02.mkv",
            "/tv/Battlestar Galactica/Season 1/Battlestar Galactica - S01E03.mkv",
        ]));
        let plan = plan(&folders, &PlanContext::default(), 7);
        assert!(plan.splits.is_empty());
        assert!(plan.new_owners.is_empty());
        assert_eq!(plan.assignments.len(), 3);
        for a in plan.assignments.values() {
            assert_eq!(a.series_key, "/tv/Battlestar Galactica");
            assert_eq!(a.series_year, Some(2003));
        }
    }

    fn flash() -> BTreeMap<String, Vec<(PathBuf, Option<u32>)>> {
        folders_of(&paths(&[
            "/tv/The Flash/Season 1/The Flash (1990) - S01E01.mkv",
            "/tv/The Flash/Season 1/The Flash (2014) - S01E01.mkv",
            "/tv/The Flash/Season 1/The Flash (2014) - S01E02.mkv",
            "/tv/The Flash/Season 1/The Flash - S01E03.mkv",
        ]))
    }

    #[test]
    fn a_folder_stating_two_years_splits_and_the_largest_group_keeps_the_key() {
        let plan = plan(&flash(), &PlanContext::default(), 7);
        assert_eq!(plan.splits.len(), 1);
        let split = &plan.splits[0];
        assert_eq!(split.owner_year, 2014);
        assert_eq!(split.owner_by, "largest");
        assert_eq!(split.groups_label(), "none:1, 1990:1, 2014:2");
        let key = |p: &str| plan.assignments[Path::new(p)].series_key.clone();
        assert_eq!(
            key("/tv/The Flash/Season 1/The Flash (2014) - S01E01.mkv"),
            "/tv/The Flash"
        );
        // An unyeared file joins the owner.
        assert_eq!(
            key("/tv/The Flash/Season 1/The Flash - S01E03.mkv"),
            "/tv/The Flash"
        );
        assert_eq!(
            key("/tv/The Flash/Season 1/The Flash (1990) - S01E01.mkv"),
            "/tv/The Flash\u{1f}1990"
        );
        assert_eq!(plan.new_owners.len(), 1);
        assert_eq!(plan.new_owners[0].target_year, Some(2014));
        assert_eq!(plan.new_owners[0].source, OverrideSource::Scanner);
        assert_eq!(plan.new_owners[0].created_at, 7);
    }

    #[test]
    fn the_stored_match_then_a_remembered_owner_decide_who_keeps_the_key() {
        let ctx = PlanContext {
            premiere_years: HashMap::from([("/tv/The Flash".to_string(), 1991)]),
            ..Default::default()
        };
        let p = plan(&flash(), &ctx, 7);
        assert_eq!(
            (p.splits[0].owner_year, p.splits[0].owner_by),
            (1990, "stored_match")
        );
        assert_eq!(p.new_owners[0].target_year, Some(1990));

        let remembered = PlanContext {
            overrides: vec![SeriesOverride {
                id: 1,
                series_folder: "/tv/The Flash".into(),
                path: None,
                target_key: "/tv/The Flash".into(),
                target_name: None,
                target_year: Some(1990),
                source: OverrideSource::Scanner,
                created_by: None,
                created_at: 1,
            }],
            // A later match against the 2014 show does not move the owner.
            premiere_years: HashMap::from([("/tv/The Flash".to_string(), 2014)]),
        };
        let p = plan(&flash(), &remembered, 7);
        assert_eq!(
            (p.splits[0].owner_year, p.splits[0].owner_by),
            (1990, "override")
        );
        assert!(p.new_owners.is_empty());
    }

    #[test]
    fn a_file_an_override_places_goes_where_it_says_admin_first() {
        let file = "/tv/The Flash/Season 1/The Flash - S01E03.mkv";
        let row = |source, year: u32| SeriesOverride {
            id: 0,
            series_folder: "/tv/The Flash".into(),
            path: Some(file.into()),
            target_key: split_key("/tv/The Flash", year),
            target_name: None,
            target_year: Some(year),
            source,
            created_by: None,
            created_at: 1,
        };
        let folders = folders_of(&paths(&[
            "/tv/The Flash/Season 3/The Flash (2014) - S03E01.mkv",
            file,
        ]));
        // A consensus folder: without the row the file joins the 2014 show.
        assert_eq!(
            plan(&folders, &PlanContext::default(), 7).assignments[Path::new(file)].series_year,
            Some(2014)
        );
        let ctx = PlanContext {
            overrides: vec![
                row(OverrideSource::Admin, 1991),
                row(OverrideSource::Scanner, 1990),
            ],
            ..Default::default()
        };
        let p = plan(&folders, &ctx, 7);
        assert_eq!(
            p.assignments[Path::new(file)],
            Assignment {
                series_key: "/tv/The Flash\u{1f}1991".into(),
                series_year: Some(1991),
            }
        );
        assert!(p.splits.is_empty());
    }

    #[test]
    fn the_diff_writes_only_changes_and_returns_a_vanished_split_to_its_folder() {
        let mut walked = flash();
        // A walked folder that no longer states two years.
        walked.extend(folders_of(&paths(&[
            "/tv/Cosmos/Season 1/Cosmos - S01E01.mkv",
        ])));
        let p = plan(&walked, &PlanContext::default(), 7);
        let stored = vec![
            row(
                "/tv/The Flash/Season 1/The Flash (1990) - S01E01.mkv",
                "/tv/The Flash",
                None,
                None,
            ),
            row(
                "/tv/The Flash/Season 1/The Flash (2014) - S01E01.mkv",
                "/tv/The Flash",
                None,
                Some(2014),
            ),
            row(
                "/tv/The Flash/Season 1/The Flash (2014) - S01E02.mkv",
                "/tv/The Flash",
                None,
                None,
            ),
            row(
                "/tv/The Flash/Season 1/The Flash - S01E03.mkv",
                "/tv/The Flash",
                None,
                None,
            ),
            // A row a split once moved, whose folder no longer states two years.
            row(
                "/tv/Cosmos/Season 1/Cosmos - S01E01.mkv",
                "/tv/Cosmos",
                Some("/tv/Cosmos\u{1f}2014"),
                Some(2014),
            ),
        ];
        let mut stored = stored;
        // A split row of a folder this walk did not see (another root's).
        stored.push(row(
            "/tv/inner/Show/S01E01.mkv",
            "/tv/inner/Show",
            Some("/tv/inner/Show\u{1f}1990"),
            Some(1990),
        ));
        let mut d = diff(&p, &stored);
        d.sort_by(|a, b| a.path.cmp(&b.path));
        let got: Vec<_> = d
            .iter()
            .map(|u| (u.path.as_str(), u.series_key.as_deref(), u.series_year))
            .collect();
        assert_eq!(
            got,
            vec![
                ("/tv/Cosmos/Season 1/Cosmos - S01E01.mkv", None, Some(2014)),
                (
                    "/tv/The Flash/Season 1/The Flash (1990) - S01E01.mkv",
                    Some("/tv/The Flash\u{1f}1990"),
                    Some(1990)
                ),
                (
                    "/tv/The Flash/Season 1/The Flash (2014) - S01E02.mkv",
                    None,
                    Some(2014)
                ),
                (
                    "/tv/The Flash/Season 1/The Flash - S01E03.mkv",
                    None,
                    Some(2014)
                ),
            ]
        );
    }
}
