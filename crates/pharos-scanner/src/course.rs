//! Video courses kept in a Books library (ADR-0126, V1392).
//!
//! A course is a folder under a Books root, its modules are folders inside
//! it, and each video is a lesson. Lessons are stored as episodes, so a
//! course opens on the show page: the course folder is the series, a
//! module's leading number its season, and the last leading number of the
//! file name its episode. Everything here reads paths only.

use std::path::{Path, PathBuf};

use pharos_core::SeriesInfo;

/// The `match_source` a lesson's row carries. A row so marked is never
/// offered to an online provider.
pub const COURSE_MATCH_SOURCE: &str = "course";

/// The `match_provider` written with [`COURSE_MATCH_SOURCE`]: the lesson's
/// metadata is its own files'.
pub const COURSE_MATCH_PROVIDER: &str = "local";

/// What a path under a Books root says about the lesson it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourseLesson {
    pub series: SeriesInfo,
    pub title: String,
}

/// The lesson `path` is, when it is a video under one of `roots`; `None`
/// for any other file, or a path under no root.
pub fn lesson(path: &Path, roots: &[PathBuf]) -> Option<CourseLesson> {
    if !crate::fs::is_film_container(path) {
        return None;
    }
    let root = roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())?;
    let parts: Vec<&str> = path
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<_>>()?;
    let stem = path.file_stem()?.to_str()?;
    let title = clean_title(stem);
    let (series_name, folder, season, episode) = match parts.as_slice() {
        [] => return None,
        // A video directly in the root is a course of one lesson.
        [_file] => (title.clone(), path.to_path_buf(), 1, Some(1)),
        [course, _file] => (
            (*course).to_owned(),
            root.join(course),
            1,
            last_leading_number(stem),
        ),
        [course, module, ..] => (
            (*course).to_owned(),
            root.join(course),
            first_leading_number(module).unwrap_or(1),
            last_leading_number(stem),
        ),
    };
    Some(CourseLesson {
        series: SeriesInfo {
            series_name,
            season_number: Some(season),
            episode_number: episode,
            series_folder: Some(folder.to_string_lossy().into_owned()),
            series_year: None,
            series_key: None,
        },
        title,
    })
}

/// A lesson's or module's name without its leading numbers and separators,
/// underscores read as spaces: `03_-_0201_Setting_up_IAM` is
/// `Setting up IAM`. A name that is nothing but numbers keeps them all.
pub fn clean_title(stem: &str) -> String {
    let (_, rest) = leading_numbers(stem);
    let words: Vec<&str> = rest
        .split(|c: char| c == '_' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect();
    let cleaned = words.join(" ");
    let cleaned = cleaned.trim_matches(is_separator).trim();
    if cleaned.is_empty() {
        stem.to_owned()
    } else {
        cleaned.to_owned()
    }
}

/// The name of the module folder `path` sits in under its course folder
/// `series_folder`, cleaned: `None` for a lesson directly in the course,
/// or a module named by a number alone.
pub fn module_title(path: &Path, series_folder: &Path) -> Option<String> {
    let rel = path.strip_prefix(series_folder).ok()?;
    let mut parts = rel.components();
    let module = parts.next()?.as_os_str().to_str()?;
    parts.next()?;
    let (_, rest) = leading_numbers(module);
    if rest
        .chars()
        .all(|c| is_separator(c) || c == '_' || c.is_whitespace())
    {
        return None;
    }
    Some(clean_title(module))
}

fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '_' | '-' | '.' | ',' | '\u{2013}' | '\u{2014}')
}

/// The numbers a name starts with, each a run of digits ending at a
/// separator or the end of the name, and the rest of the name after them.
/// A run that runs into a letter (`3D`) or past `u32` is not a number.
fn leading_numbers(name: &str) -> (Vec<u32>, &str) {
    let mut numbers = Vec::new();
    let mut rest = name.trim_start_matches(is_separator);
    loop {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            break;
        }
        let (run, after) = rest.split_at(digits);
        if after.chars().next().is_some_and(|c| !is_separator(c)) {
            break;
        }
        let Ok(n) = run.parse::<u32>() else {
            break;
        };
        numbers.push(n);
        rest = after.trim_start_matches(is_separator);
    }
    (numbers, rest)
}

fn first_leading_number(name: &str) -> Option<u32> {
    leading_numbers(name).0.first().copied()
}

fn last_leading_number(name: &str) -> Option<u32> {
    leading_numbers(name).0.last().copied()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/lib/Books")]
    }

    fn read(path: &str) -> Option<(String, String, u32, Option<u32>, String)> {
        lesson(Path::new(path), &roots()).map(|l| {
            (
                l.series.series_name,
                l.series.series_folder.unwrap(),
                l.series.season_number.unwrap(),
                l.series.episode_number,
                l.title,
            )
        })
    }

    #[test]
    fn each_course_layout_reads_as_lessons() {
        let cases = [
            (
                "/lib/Books/Intro to Widgets/02 - Assembly/03_-_0201_Fitting_the_Base.mp4",
                ("Intro to Widgets", 2, Some(201), "Fitting the Base"),
            ),
            (
                "/lib/Books/Intro to Widgets/02. Assembly/02_05-Fitting.mp4",
                ("Intro to Widgets", 2, Some(5), "Fitting"),
            ),
            (
                "/lib/Books/Intro to Widgets/03/007 Fitting.mp4",
                ("Intro to Widgets", 3, Some(7), "Fitting"),
            ),
            (
                "/lib/Books/Intro to Widgets/01 - Getting Started.mp4",
                ("Intro to Widgets", 1, Some(1), "Getting Started"),
            ),
            (
                "/lib/Books/Intro to Widgets/Bonus/Wrapping Up.mkv",
                ("Intro to Widgets", 1, None, "Wrapping Up"),
            ),
        ];
        for (path, (course, season, episode, title)) in cases {
            let (name, folder, s, e, t) = read(path).unwrap_or_else(|| panic!("{path}"));
            assert_eq!(name, course, "{path}");
            assert_eq!(folder, "/lib/Books/Intro to Widgets", "{path}");
            assert_eq!((s, e), (season, episode), "{path}");
            assert_eq!(t, title, "{path}");
        }
    }

    #[test]
    fn a_video_directly_in_the_root_is_a_course_of_one_lesson() {
        let (name, folder, season, episode, title) =
            read("/lib/Books/05 - Widget Care.mp4").unwrap();
        assert_eq!(name, "Widget Care");
        assert_eq!(folder, "/lib/Books/05 - Widget Care.mp4");
        assert_eq!((season, episode), (1, Some(1)));
        assert_eq!(title, "Widget Care");
    }

    #[test]
    fn a_lesson_has_no_year_or_split_key() {
        let l = lesson(
            Path::new("/lib/Books/Intro to Widgets (2021)/01 - Start.mp4"),
            &roots(),
        )
        .unwrap();
        assert_eq!(l.series.series_name, "Intro to Widgets (2021)");
        assert_eq!(l.series.series_year, None);
        assert_eq!(l.series.series_key, None);
    }

    #[test]
    fn only_a_video_under_a_books_root_is_a_lesson() {
        for path in [
            "/lib/TV/Show A/Season 01/Show A - S01E01.mkv",
            "/lib/Booksellers/Intro to Widgets/01 - Start.mp4",
            "/lib/Books/Intro to Widgets/Widgets.epub",
            "/lib/Books/Intro to Widgets/01 - Start.mp3",
            "/lib/Books/Intro to Widgets/notes.html",
        ] {
            assert_eq!(read(path), None, "{path}");
        }
        assert_eq!(
            lesson(Path::new("/lib/Books/Intro to Widgets/01 - Start.mp4"), &[]),
            None
        );
    }

    #[test]
    fn a_nested_root_takes_its_own_files() {
        let roots = vec![PathBuf::from("/lib"), PathBuf::from("/lib/Books")];
        let l = lesson(
            Path::new("/lib/Books/Intro to Widgets/01 - Start.mp4"),
            &roots,
        )
        .unwrap();
        assert_eq!(l.series.series_name, "Intro to Widgets");
    }

    #[test]
    fn a_title_loses_its_numbers_and_separators() {
        for (stem, want) in [
            ("03_-_0201_Setting_up_Widgets", "Setting up Widgets"),
            ("02_05-Fitting", "Fitting"),
            ("007 Fitting", "Fitting"),
            ("01 - Getting Started", "Getting Started"),
            ("1. Intro", "Intro"),
            ("3D Widgets", "3D Widgets"),
            ("Getting Started", "Getting Started"),
            ("01", "01"),
            ("01 - ", "01 - "),
        ] {
            assert_eq!(clean_title(stem), want, "{stem}");
        }
    }

    #[test]
    fn a_module_is_named_by_its_folder() {
        let course = Path::new("/lib/Books/Intro to Widgets");
        let module = |p: &str| module_title(&course.join(p), course);
        assert_eq!(
            module("02 - Assembly/01 - Base.mp4").as_deref(),
            Some("Assembly")
        );
        assert_eq!(
            module("02. Assembly/01 - Base.mp4").as_deref(),
            Some("Assembly")
        );
        assert_eq!(module("03/007 Fitting.mp4"), None);
        assert_eq!(module("01 - Getting Started.mp4"), None);
        assert_eq!(
            module_title(Path::new("/lib/Books/Other/02 - Assembly/01.mp4"), course),
            None
        );
    }

    proptest::proptest! {
        #[test]
        fn a_lesson_title_is_never_empty(stem in "\\PC{1,40}") {
            proptest::prop_assert!(!clean_title(&stem).is_empty());
        }

        #[test]
        fn every_video_under_the_root_is_a_titled_lesson(
            course in "[A-Za-z0-9 ._-]{1,20}",
            module in "[A-Za-z0-9 ._-]{1,20}",
            stem in "[A-Za-z0-9 ._-]{1,30}",
        ) {
            proptest::prop_assume!(![course.as_str(), module.as_str()].iter().any(|p| matches!(*p, "." | "..")));
            let path = PathBuf::from("/lib/Books").join(&course).join(&module).join(format!("{stem}.mp4"));
            proptest::prop_assume!(path.extension().and_then(|e| e.to_str()) == Some("mp4"));
            let got = lesson(&path, &roots());
            proptest::prop_assert!(got.is_some(), "{path:?}");
            let got = got.unwrap();
            proptest::prop_assert!(!got.title.is_empty());
            proptest::prop_assert!(got.series.season_number.is_some());
        }
    }
}
