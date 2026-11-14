//! Audiobooks kept in a Books library (ADR-0132, spec 046).
//!
//! An audiobook is a single file with chapters (any `.m4b`, or an `.m4a` /
//! `.mp3` that carries chapter marks) or a folder of audio files under a
//! Books root. Each file is stored as an `Audio` row whose album is the book:
//! a single file is an album of one track, its chapters the file's own; a
//! folder's files are its tracks, numbered in natural order, and each track
//! is a chapter. Which library holds a file is read as courses read it
//! ([`CourseRoots`]): only a file whose innermost library is a Books library
//! is a book.

use std::path::{Path, PathBuf};

use pharos_core::MediaProbe;

use crate::course::CourseRoots;

/// Audio containers a folder book's tracks may be. `.m4b` is not among
/// them: an `.m4b` is always a book of its own.
const TRACK_EXTENSIONS: &[&str] = &["mp3", "m4a", "flac", "ogg", "opus", "wav"];

/// The extension that makes a file a single-file book whatever it holds.
const BOOK_EXTENSION: &str = "m4b";

/// How a file under a Books root was read: the `verdict` label of
/// `pharos_scan_audiobook_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The file is a whole book.
    SingleFile,
    /// The file is one track of a folder book.
    FolderTrack,
    /// The file is not a book and is left out of the library.
    Skipped,
}

impl Verdict {
    pub const ALL: [Self; 3] = [Self::SingleFile, Self::FolderTrack, Self::Skipped];

    pub fn label(self) -> &'static str {
        match self {
            Self::SingleFile => "single_file",
            Self::FolderTrack => "folder_track",
            Self::Skipped => "skipped",
        }
    }
}

/// Why a file got its [`Verdict`]: the `reason` label of
/// `pharos_scan_audiobook_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// An `.m4b`, a book by its container.
    M4b,
    /// Alone in its folder (or at the root) and it carries chapter marks.
    Chapters,
    /// One of the audio files in a book folder.
    InFolder,
    /// Directly in the Books root with no chapter marks: a loose file, not
    /// a book.
    LooseWithoutChapters,
}

impl Reason {
    pub const ALL: [Self; 4] = [
        Self::M4b,
        Self::Chapters,
        Self::InFolder,
        Self::LooseWithoutChapters,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::M4b => "m4b",
            Self::Chapters => "chapters",
            Self::InFolder => "in_folder",
            Self::LooseWithoutChapters => "loose_without_chapters",
        }
    }
}

/// What an audio file under a Books root is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub verdict: Verdict,
    pub reason: Reason,
    /// The book this file belongs to; `None` when skipped.
    pub book: Option<BookTrack>,
}

/// One file's place in its book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookTrack {
    /// The book's title, stored as the track's album.
    pub title: String,
    /// The book's author, stored as the track's album artist.
    pub author: Option<String>,
    /// The track's title.
    pub track_title: String,
    /// Its place among the folder's tracks, from 1; `None` for a single file.
    pub track: Option<u32>,
    /// How many tracks the folder holds; 1 for a single file.
    pub tracks: u32,
    /// The book's folder, or the file itself for a single file.
    pub folder: PathBuf,
}

/// Whether `path` is held in a container a book can be: what the scanner
/// must walk and probe for a book to be found.
pub fn is_book_container(path: &Path) -> bool {
    extension(path).is_some_and(|e| e == BOOK_EXTENSION || TRACK_EXTENSIONS.contains(&e.as_str()))
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

fn is_track_file(path: &Path) -> bool {
    let hidden = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'));
    !hidden && extension(path).is_some_and(|e| TRACK_EXTENSIONS.contains(&e.as_str()))
}

/// A folder that only splits a book into discs (`CD1`, `Disc 02`,
/// `Part 3`), so the book is the folder above it.
fn is_disc_folder(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some(rest) = ["cd", "disc", "disk", "part"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
    else {
        return false;
    };
    let digits = rest.trim_start_matches([' ', '_', '-', '.']);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// The folder a track in `parent` belongs to as a book: `parent` itself,
/// or the folder above a disc folder while that is still inside the root.
fn book_folder(parent: &Path, root: &Path) -> PathBuf {
    let disc = parent
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_disc_folder);
    match parent.parent() {
        Some(above) if disc && above != root && above.starts_with(root) => above.to_path_buf(),
        _ => parent.to_path_buf(),
    }
}

/// The track files of the book in `folder`, every folder below it
/// included, in natural order of their paths under it.
pub fn folder_tracks(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![folder.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(folder = %dir.display(), error = %e, "audiobook: a book folder could not be listed");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(path),
                Ok(_) if is_track_file(&path) => found.push(path),
                _ => {}
            }
        }
    }
    found.sort_by(|a, b| {
        let key = |p: &PathBuf| {
            p.strip_prefix(folder)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        };
        crate::book::comic::natural_cmp(&key(a), &key(b))
    });
    found
}

fn tag(value: Option<&String>) -> Option<String> {
    value
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn folder_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|n| crate::course::clean_title(&n.to_string_lossy()))
}

/// The author a book's tags name (album artist, else artist), else the
/// folder its book sits in when that is not the root (`Author/Title/`).
fn author(probe: &MediaProbe, book: &Path, root: &Path) -> Option<String> {
    tag(probe.album_artist.as_ref())
        .or_else(|| tag(probe.artist.as_ref()))
        .or_else(|| {
            book.parent()
                .filter(|p| *p != root && p.starts_with(root))
                .and_then(folder_name)
        })
}

fn stem_title(path: &Path) -> String {
    path.file_stem().map_or_else(
        || "unknown".to_owned(),
        |s| crate::course::clean_title(&s.to_string_lossy()),
    )
}

/// What the audio file `path`, probed as `probe`, is under `roots`; `None`
/// when it is not an audio file a book can be, or no Books library is the
/// innermost one holding it. `tracks` lists a book folder's tracks
/// ([`folder_tracks`]); it is asked only for a file in a folder.
pub fn classify(
    path: &Path,
    probe: &MediaProbe,
    roots: &CourseRoots,
    tracks: impl FnOnce(&Path) -> Vec<PathBuf>,
) -> Option<Classified> {
    if !is_book_container(path) {
        return None;
    }
    let root = roots.root_of(path)?;
    let parent = path.parent()?;
    let has_chapters = !probe.chapters.is_empty();
    let single = |reason| {
        let title = tag(probe.album.as_ref())
            .or_else(|| tag(probe.title.as_ref()))
            .unwrap_or_else(|| stem_title(path));
        Classified {
            verdict: Verdict::SingleFile,
            reason,
            book: Some(BookTrack {
                author: author(probe, path, root),
                track_title: title.clone(),
                title,
                track: None,
                tracks: 1,
                folder: path.to_path_buf(),
            }),
        }
    };
    if extension(path).as_deref() == Some(BOOK_EXTENSION) {
        return Some(single(Reason::M4b));
    }
    if parent == root {
        return Some(if has_chapters {
            single(Reason::Chapters)
        } else {
            Classified {
                verdict: Verdict::Skipped,
                reason: Reason::LooseWithoutChapters,
                book: None,
            }
        });
    }
    let folder = book_folder(parent, root);
    let listed = tracks(&folder);
    if has_chapters && listed.len() <= 1 {
        return Some(single(Reason::Chapters));
    }
    let at = listed.iter().position(|p| p == path);
    let count = u32::try_from(listed.len().max(1)).unwrap_or(u32::MAX);
    Some(Classified {
        verdict: Verdict::FolderTrack,
        reason: Reason::InFolder,
        book: Some(BookTrack {
            title: folder_name(&folder).unwrap_or_else(|| stem_title(&folder)),
            author: author(probe, &folder, root),
            track_title: tag(probe.title.as_ref()).unwrap_or_else(|| stem_title(path)),
            track: at
                .and_then(|i| u32::try_from(i).ok())
                .map(|i| i.saturating_add(1)),
            tracks: count,
            folder,
        }),
    })
}

impl BookTrack {
    /// Write this track's book onto its probe: the book is the album, its
    /// author the album artist, and its place the track number, with no
    /// disc, so an album's track order is the book's.
    pub fn apply(&self, probe: &mut MediaProbe) {
        probe.album = Some(self.title.clone());
        if let Some(author) = &self.author {
            probe.album_artist = Some(author.clone());
        }
        probe.track_number = self.track.or(Some(1));
        probe.disc_number = None;
    }
}

/// Log and count what a file under a Books root was read as.
pub fn record(path: &Path, probe: &MediaProbe, got: &Classified) {
    let book = got.book.as_ref();
    tracing::info!(
        path = %path.display(),
        verdict = got.verdict.label(),
        reason = got.reason.label(),
        chapters = probe.chapters.len(),
        book = book.map_or("", |b| b.title.as_str()),
        author = book.and_then(|b| b.author.as_deref()).unwrap_or(""),
        track = book.and_then(|b| b.track),
        tracks = book.map_or(0, |b| b.tracks),
        "scanner: audio in a books library classified"
    );
    metrics::counter!(
        "pharos_scan_audiobook_total",
        "verdict" => got.verdict.label(),
        "reason" => got.reason.label()
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use pharos_core::MediaChapter;
    use std::collections::HashSet;

    const ROOT: &str = "/lib/Books";

    fn roots() -> CourseRoots {
        CourseRoots::new([
            (PathBuf::from(ROOT), true),
            (PathBuf::from("/lib/Books/Music"), false),
        ])
    }

    fn chapters(n: u64) -> Vec<MediaChapter> {
        (0..n)
            .map(|i| MediaChapter {
                start_ms: i * 1000,
                end_ms: (i + 1) * 1000,
                title: format!("Chapter {}", i + 1),
            })
            .collect()
    }

    fn listed<'a>(names: &'a [&'a str]) -> impl FnOnce(&Path) -> Vec<PathBuf> + 'a {
        move |folder| names.iter().map(|n| folder.join(n)).collect()
    }

    fn never(_: &Path) -> Vec<PathBuf> {
        panic!("a single file's folder is not listed")
    }

    #[test]
    fn an_m4b_is_a_book_whatever_it_holds() {
        let probe = MediaProbe {
            album: Some("Gadget Tales".into()),
            album_artist: Some("Ada Field".into()),
            ..Default::default()
        };
        let got = classify(
            Path::new("/lib/Books/Ada Field/Gadget Tales.m4b"),
            &probe,
            &roots(),
            never,
        )
        .unwrap();
        assert_eq!(
            (got.verdict, got.reason),
            (Verdict::SingleFile, Reason::M4b)
        );
        let book = got.book.unwrap();
        assert_eq!(book.title, "Gadget Tales");
        assert_eq!(book.author.as_deref(), Some("Ada Field"));
        assert_eq!((book.track, book.tracks), (None, 1));
    }

    #[test]
    fn a_single_file_with_chapters_is_a_book_and_its_name_fills_gaps() {
        let probe = MediaProbe {
            chapters: chapters(3),
            ..Default::default()
        };
        let got = classify(
            Path::new("/lib/Books/Ada Field/02 - Gadget Tales.mp3"),
            &probe,
            &roots(),
            listed(&["02 - Gadget Tales.mp3"]),
        )
        .unwrap();
        assert_eq!(
            (got.verdict, got.reason),
            (Verdict::SingleFile, Reason::Chapters)
        );
        let book = got.book.unwrap();
        assert_eq!(book.title, "Gadget Tales");
        assert_eq!(
            book.author.as_deref(),
            Some("Ada Field"),
            "the folder above names the author"
        );

        let at_root = classify(
            Path::new("/lib/Books/Gadget Tales.m4a"),
            &probe,
            &roots(),
            never,
        )
        .unwrap();
        assert_eq!(at_root.verdict, Verdict::SingleFile);
        assert_eq!(at_root.book.unwrap().author, None);
    }

    #[test]
    fn a_loose_file_at_the_root_without_chapters_is_skipped() {
        let got = classify(
            Path::new("/lib/Books/Stray.mp3"),
            &MediaProbe::default(),
            &roots(),
            never,
        )
        .unwrap();
        assert_eq!(
            (got.verdict, got.reason),
            (Verdict::Skipped, Reason::LooseWithoutChapters)
        );
        assert_eq!(got.book, None);
    }

    #[test]
    fn a_folder_of_files_is_one_book_in_natural_order() {
        let names = ["1 Opening.mp3", "2 Middle.mp3", "10 Ending.mp3"];
        let mut sorted: Vec<&str> = names.to_vec();
        sorted.sort_by(|a, b| crate::book::comic::natural_cmp(a, b));
        assert_eq!(sorted, names, "natural order puts 10 after 2");
        let path = Path::new("/lib/Books/Ada Field/Gadget Tales/10 Ending.mp3");
        let got = classify(path, &MediaProbe::default(), &roots(), listed(&names)).unwrap();
        assert_eq!(
            (got.verdict, got.reason),
            (Verdict::FolderTrack, Reason::InFolder)
        );
        let book = got.book.unwrap();
        assert_eq!(book.title, "Gadget Tales");
        assert_eq!(book.author.as_deref(), Some("Ada Field"));
        assert_eq!(book.track_title, "Ending");
        assert_eq!((book.track, book.tracks), (Some(3), 3));
        assert_eq!(book.folder, Path::new("/lib/Books/Ada Field/Gadget Tales"));
    }

    #[test]
    fn a_disc_folder_belongs_to_the_book_above_it() {
        let path = Path::new("/lib/Books/Gadget Tales/CD2/01.mp3");
        let mut asked = None;
        let got = classify(path, &MediaProbe::default(), &roots(), |f: &Path| {
            asked = Some(f.to_path_buf());
            vec![f.join("CD1/01.mp3"), f.join("CD2/01.mp3")]
        })
        .unwrap();
        assert_eq!(asked.as_deref(), Some(Path::new("/lib/Books/Gadget Tales")));
        let book = got.book.unwrap();
        assert_eq!(book.title, "Gadget Tales");
        assert_eq!(book.author, None);
        assert_eq!(book.track, Some(2));
        for (name, disc) in [
            ("CD1", true),
            ("Disc 02", true),
            ("part_3", true),
            ("Parts", false),
            ("CDs", false),
            ("Discovery", false),
        ] {
            assert_eq!(is_disc_folder(name), disc, "{name}");
        }
    }

    #[test]
    fn tracks_with_chapters_in_a_shared_folder_stay_one_book() {
        let probe = MediaProbe {
            chapters: chapters(2),
            ..Default::default()
        };
        let got = classify(
            Path::new("/lib/Books/Gadget Tales/01.mp3"),
            &probe,
            &roots(),
            listed(&["01.mp3", "02.mp3"]),
        )
        .unwrap();
        assert_eq!(got.verdict, Verdict::FolderTrack);
    }

    #[test]
    fn tags_name_the_author_before_folders() {
        let probe = MediaProbe {
            artist: Some("Ada Field".into()),
            title: Some("Part the First".into()),
            ..Default::default()
        };
        let book = classify(
            Path::new("/lib/Books/Someone Else/Gadget Tales/01.mp3"),
            &probe,
            &roots(),
            listed(&["01.mp3"]),
        )
        .unwrap()
        .book
        .unwrap();
        assert_eq!(book.author.as_deref(), Some("Ada Field"));
        assert_eq!(book.track_title, "Part the First");
    }

    #[test]
    fn only_audio_under_an_innermost_books_library_is_read() {
        for path in [
            "/lib/Books/Music/Band/Album/01.mp3",
            "/lib/Media/Gadget Tales.m4b",
            "/lib/Books/Gadget Tales/01.mp4",
            "/lib/Books/Gadget Tales.epub",
        ] {
            assert_eq!(
                classify(Path::new(path), &MediaProbe::default(), &roots(), never),
                None,
                "{path}"
            );
        }
    }

    #[test]
    fn apply_makes_the_book_the_album() {
        let mut probe = MediaProbe {
            album: Some("Some Rip (Disc 1)".into()),
            disc_number: Some(1),
            track_number: Some(7),
            ..Default::default()
        };
        BookTrack {
            title: "Gadget Tales".into(),
            author: Some("Ada Field".into()),
            track_title: "Opening".into(),
            track: Some(2),
            tracks: 3,
            folder: PathBuf::from("/lib/Books/Gadget Tales"),
        }
        .apply(&mut probe);
        assert_eq!(probe.album.as_deref(), Some("Gadget Tales"));
        assert_eq!(probe.album_artist.as_deref(), Some("Ada Field"));
        assert_eq!((probe.disc_number, probe.track_number), (None, Some(2)));
    }

    #[test]
    fn folder_tracks_lists_audio_below_in_natural_order() {
        let td = tempfile::TempDir::new().unwrap();
        for f in [
            "CD1/2 b.mp3",
            "CD1/10 c.mp3",
            "CD1/1 a.mp3",
            "CD2/1 d.mp3",
            "cover.jpg",
            ".hidden.mp3",
            "x.m4b",
        ] {
            let p = td.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"").unwrap();
        }
        let got: Vec<String> = folder_tracks(td.path())
            .iter()
            .map(|p| {
                p.strip_prefix(td.path())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            got,
            ["CD1/1 a.mp3", "CD1/2 b.mp3", "CD1/10 c.mp3", "CD2/1 d.mp3"]
        );
    }

    #[test]
    fn labels_are_distinct() {
        let v: HashSet<_> = Verdict::ALL.iter().map(|v| v.label()).collect();
        assert_eq!(v.len(), Verdict::ALL.len());
        let r: HashSet<_> = Reason::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(r.len(), Reason::ALL.len());
    }
}
