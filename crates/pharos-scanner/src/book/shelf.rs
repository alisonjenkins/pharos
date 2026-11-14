//! 043-book-categories — raw subject strings to a small fixed set of shelves.
//!
//! A book's subjects come from its file (`dc:subject`, EXTH 105, `/Keywords`)
//! or from Open Library (ADR-0129). Both are free text: BISAC paths
//! (`COMPUTERS / Programming / General`), Library of Congress headings
//! (`Python (Computer program language)`), calibre tags, catalogue noise. The
//! Books grid filters on genres, and a filter of four hundred one-off strings
//! is no filter, so each subject is mapped to one of [`Shelf::ALL`] and the
//! shelves are what is stored as genres.
//!
//! # The rule
//!
//! Each subject goes to the FIRST shelf, in [`Shelf::ALL`] order, with a
//! whole-word keyword match. The order puts the specific shelves before the
//! general ones, so `JavaScript (Computer program language)` is Web, not
//! Programming, and `Amazon Web Services` is Cloud & DevOps, not Web. Fiction
//! is first so `Science fiction` is not Maths & Science.
//!
//! A book's shelves are the top two by subject count, the second only when it
//! has at least half the first's count; ties go to table order. Subjects that
//! match no shelf give [`Shelf::Other`]. Known catalogue noise is dropped
//! before anything is counted, so a book whose only subjects are noise has no
//! usable subjects at all.
//!
//! # Signal
//!
//! `pharos_book_category_total{source,outcome}`: one per decision, from the
//! scan (`embedded`) and from the Open Library job (`openlibrary`). The raw
//! subjects and the chosen shelves go in the log line beside it.

/// One shelf. The display names are stored as genre names, so they are a
/// data contract: renaming one orphans every book already on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shelf {
    Fiction,
    Security,
    DataAi,
    CloudDevOps,
    Web,
    Electronics,
    Programming,
    MathsScience,
    Business,
    SelfHelp,
    Other,
}

impl Shelf {
    /// Matching order: specific before general. [`Shelf::Other`] is the
    /// fallback and has no keywords.
    pub const ALL: [Self; 11] = [
        Self::Fiction,
        Self::Security,
        Self::DataAi,
        Self::CloudDevOps,
        Self::Web,
        Self::Electronics,
        Self::Programming,
        Self::MathsScience,
        Self::Business,
        Self::SelfHelp,
        Self::Other,
    ];

    /// The genre name the shelf is stored as.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fiction => "Fiction",
            Self::Security => "Security",
            Self::DataAi => "Data & AI",
            Self::CloudDevOps => "Cloud & DevOps",
            Self::Web => "Web",
            Self::Electronics => "Electronics & Making",
            Self::Programming => "Programming",
            Self::MathsScience => "Maths & Science",
            Self::Business => "Business & Productivity",
            Self::SelfHelp => "Self-help",
            Self::Other => "Other",
        }
    }

    /// Whole-word keywords, already in [`normalise`]d form.
    const fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Fiction => &[
                "fiction",
                "novel",
                "novels",
                "fantasy",
                "thriller",
                "thrillers",
                "short stories",
                "mystery",
            ],
            Self::Security => &[
                "security",
                "cybersecurity",
                "hacking",
                "hackers",
                "penetration testing",
                "cryptography",
                "encryption",
                "malware",
            ],
            Self::DataAi => &[
                "machine learning",
                "artificial intelligence",
                "deep learning",
                "neural networks",
                "neural network",
                "data science",
                "data mining",
                "big data",
                "data analysis",
                "statistics",
                "natural language processing",
                "tensorflow",
                "pytorch",
                "ai",
                "llm",
                "large language models",
                "database",
                "databases",
                "sql",
            ],
            Self::CloudDevOps => &[
                "cloud",
                "cloud computing",
                "aws",
                "amazon web services",
                "azure",
                "devops",
                "docker",
                "kubernetes",
                "terraform",
                "ansible",
                "site reliability engineering",
                "sre",
                "continuous integration",
                "continuous delivery",
                "infrastructure as code",
                "microservices",
                "serverless",
                "linux",
                "unix",
                "system administration",
                "systems administration",
            ],
            Self::Web => &[
                "web",
                "html",
                "html5",
                "css",
                "javascript",
                "typescript",
                "react",
                "angular",
                "vue",
                "node js",
                "nodejs",
                "frontend",
                "front end",
                "websites",
                "web sites",
                "php",
                "django",
                "rails",
                "graphql",
                "world wide web",
            ],
            Self::Electronics => &[
                "electronics",
                "arduino",
                "raspberry pi",
                "microcontrollers",
                "microcontroller",
                "embedded systems",
                "embedded computer systems",
                "circuits",
                "soldering",
                "robotics",
                "robots",
                "3d printing",
                "maker",
                "makers",
                "diy",
                "internet of things",
                "iot",
                "esp32",
                "single board computers",
            ],
            Self::Programming => &[
                "programming",
                "computer programming",
                "programming languages",
                "software",
                "software engineering",
                "software development",
                "computer science",
                "algorithms",
                "data structures",
                "python",
                "rust",
                "java",
                "c++",
                "c#",
                "golang",
                "kotlin",
                "swift",
                "haskell",
                "clojure",
                "elixir",
                "erlang",
                "scala",
                "ruby",
                "perl",
                "design patterns",
                "refactoring",
                "debugging",
                "git",
                "version control",
                "coding",
                "computers",
            ],
            Self::MathsScience => &[
                "mathematics",
                "math",
                "maths",
                "algebra",
                "calculus",
                "geometry",
                "physics",
                "chemistry",
                "biology",
                "astronomy",
                "science",
            ],
            Self::Business => &[
                "business",
                "management",
                "time management",
                "productivity",
                "leadership",
                "entrepreneurship",
                "startups",
                "marketing",
                "economics",
                "finance",
                "investing",
                "career",
                "careers",
                "project management",
                "agile",
                "scrum",
                "negotiation",
            ],
            Self::SelfHelp => &[
                "self help",
                "self actualization",
                "self improvement",
                "self realization",
                "personal development",
                "motivation",
                "habit",
                "habits",
                "happiness",
                "success",
                "mindfulness",
                "psychology",
                "conduct of life",
                "stress",
                "meditation",
                "well being",
                "wellbeing",
            ],
            Self::Other => &[],
        }
    }
}

/// Catalogue noise: true of the copy or the catalogue, not of the book.
/// Matched against the whole normalised subject, or its start.
const NOISE: &[&str] = &[
    "accessible book",
    "protected daisy",
    "in library",
    "lending library",
    "large type books",
    "electronic books",
    "ebooks",
    "ebook",
    "open library",
    "overdrive",
    "nyt",
    "general",
];

/// Most raw subjects kept as tags.
pub const MAX_TAGS: usize = 20;
/// Longest raw subject kept, in characters. Longer is a blurb in the wrong
/// field.
const MAX_SUBJECT_CHARS: usize = 120;
/// Longest entry a `/Keywords` list may carry and still read as a list.
const MAX_KEYWORD_CHARS: usize = 40;
/// Most shelves one book is put on.
const MAX_SHELVES: usize = 2;

/// Lowercase, every character that is not alphanumeric, `+` or `#` as a
/// space, whitespace collapsed. `+` and `#` survive so `C++` and `C#` match.
pub fn normalise(s: &str) -> String {
    let mapped: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| {
            if c.is_alphanumeric() || c == '+' || c == '#' {
                c
            } else {
                ' '
            }
        })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_noise(normalised: &str) -> bool {
    NOISE
        .iter()
        .any(|n| normalised == *n || normalised.starts_with(&format!("{n} ")))
}

/// The subjects worth reading: trimmed, blank, overlong and noise entries
/// dropped, de-duplicated ignoring case, in their original order.
pub fn usable_subjects<S: AsRef<str>>(raw: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in raw {
        let t = s.as_ref().trim();
        if t.is_empty() || t.chars().count() > MAX_SUBJECT_CHARS {
            continue;
        }
        let n = normalise(t);
        if n.is_empty() || is_noise(&n) {
            continue;
        }
        if !out.iter().any(|e| e.eq_ignore_ascii_case(t)) {
            out.push(t.to_owned());
        }
    }
    out
}

/// The shelf one subject goes on, `None` when no keyword matches.
pub fn shelf_of(subject: &str) -> Option<Shelf> {
    let padded = format!(" {} ", normalise(subject));
    Shelf::ALL.into_iter().find(|shelf| {
        shelf
            .keywords()
            .iter()
            .any(|kw| padded.contains(&format!(" {kw} ")))
    })
}

/// A book's shelves from its subjects. Empty when there is no usable subject;
/// `[Other]` when there are subjects and none matches a shelf.
pub fn shelves_for<S: AsRef<str>>(raw: &[S]) -> Vec<Shelf> {
    let subjects = usable_subjects(raw);
    if subjects.is_empty() {
        return Vec::new();
    }
    let mut counts: Vec<(Shelf, usize)> = Vec::new();
    for shelf in subjects.iter().filter_map(|s| shelf_of(s)) {
        match counts.iter_mut().find(|(s, _)| *s == shelf) {
            Some((_, n)) => *n = n.saturating_add(1),
            None => counts.push((shelf, 1)),
        }
    }
    if counts.is_empty() {
        return vec![Shelf::Other];
    }
    let order = |s: Shelf| {
        Shelf::ALL
            .iter()
            .position(|x| *x == s)
            .unwrap_or(usize::MAX)
    };
    counts.sort_by(|(a, na), (b, nb)| nb.cmp(na).then(order(*a).cmp(&order(*b))));
    let top = counts.first().map_or(0, |(_, n)| *n);
    counts
        .into_iter()
        .take(MAX_SHELVES)
        .filter(|(_, n)| n.saturating_mul(2) >= top)
        .map(|(s, _)| s)
        .collect()
}

/// Whether a stored genre list is only the fallback shelf, or empty: such a
/// book is still worth looking up.
pub fn needs_lookup<S: AsRef<str>>(genres: &[S]) -> bool {
    genres
        .iter()
        .all(|g| g.as_ref().trim().is_empty() || g.as_ref() == Shelf::Other.name())
}

/// A PDF's `/Keywords` as subjects, when it reads as a list: two or more
/// entries split on `,` or `;`, each at most [`MAX_KEYWORD_CHARS`]. A
/// sentence in that field is a blurb, and its words would shelve the book on
/// whatever it happens to mention.
pub fn pdf_keyword_list(raw: &str) -> Vec<String> {
    let parts: Vec<String> = raw
        .split([',', ';'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    if parts.len() < 2 || parts.iter().any(|p| p.chars().count() > MAX_KEYWORD_CHARS) {
        return Vec::new();
    }
    parts
}

/// Where a book's subjects came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The book file itself, at scan time.
    Embedded,
    /// The Open Library job (ADR-0129).
    OpenLibrary,
}

impl Source {
    pub const ALL: [Self; 2] = [Self::Embedded, Self::OpenLibrary];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::OpenLibrary => "openlibrary",
        }
    }
}

/// How one categorisation decision ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// At least one real shelf.
    Shelved,
    /// Subjects, but none matched a shelf: the book is on `Other`.
    Other,
    /// The source gave no usable subject.
    NoSubjects,
    /// Open Library had no confident match.
    NoMatch,
    /// Open Library could not be asked (transport, HTTP status, decode).
    Unavailable,
    /// An admin locked the book's genres; nothing was written.
    Locked,
}

impl Outcome {
    pub const ALL: [Self; 6] = [
        Self::Shelved,
        Self::Other,
        Self::NoSubjects,
        Self::NoMatch,
        Self::Unavailable,
        Self::Locked,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Shelved => "shelved",
            Self::Other => "other",
            Self::NoSubjects => "no_subjects",
            Self::NoMatch => "no_match",
            Self::Unavailable => "unavailable",
            Self::Locked => "locked",
        }
    }

    /// The outcome of mapping subjects to `shelves`.
    pub fn of_shelves(shelves: &[Shelf]) -> Self {
        match shelves {
            [] => Self::NoSubjects,
            [Shelf::Other] => Self::Other,
            _ => Self::Shelved,
        }
    }
}

/// Count one categorisation decision.
pub fn record(source: Source, outcome: Outcome) {
    metrics::counter!(
        "pharos_book_category_total",
        "source" => source.label(),
        "outcome" => outcome.label(),
    )
    .increment(1);
}

/// The genre names of `shelves`.
pub fn names(shelves: &[Shelf]) -> Vec<String> {
    shelves.iter().map(|s| s.name().to_owned()).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn each_subject_goes_on_its_shelf() {
        let cases: &[(&str, Option<Shelf>)] = &[
            (
                "COMPUTERS / Programming / General",
                Some(Shelf::Programming),
            ),
            (
                "Python (Computer program language)",
                Some(Shelf::Programming),
            ),
            ("C++ (Computer program language)", Some(Shelf::Programming)),
            ("JavaScript (Computer program language)", Some(Shelf::Web)),
            ("Web site development", Some(Shelf::Web)),
            ("Amazon Web Services (Firm)", Some(Shelf::CloudDevOps)),
            ("Cloud computing", Some(Shelf::CloudDevOps)),
            ("Kubernetes", Some(Shelf::CloudDevOps)),
            ("Linux", Some(Shelf::CloudDevOps)),
            ("Machine learning", Some(Shelf::DataAi)),
            ("Artificial intelligence", Some(Shelf::DataAi)),
            ("Computer security", Some(Shelf::Security)),
            ("Raspberry Pi (Computer)", Some(Shelf::Electronics)),
            (
                "Arduino (Programmable controller)",
                Some(Shelf::Electronics),
            ),
            ("Mathematics", Some(Shelf::MathsScience)),
            ("Science fiction", Some(Shelf::Fiction)),
            ("Fiction, general", Some(Shelf::Fiction)),
            ("Time management", Some(Shelf::Business)),
            ("Success in business", Some(Shelf::Business)),
            ("Self-help techniques", Some(Shelf::SelfHelp)),
            ("Habit breaking", Some(Shelf::SelfHelp)),
            ("Cooking", None),
            // Whole words only: "javanese" is not Java, "nonfiction" is not
            // Fiction, "aide" is not AI.
            ("Javanese literature", None),
            ("Nonfiction", None),
            ("Teacher's aide", None),
        ];
        for (subject, want) in cases {
            assert_eq!(shelf_of(subject), *want, "{subject}");
        }
    }

    #[test]
    fn a_book_gets_its_top_shelves() {
        assert_eq!(
            shelves_for(&["Python", "Computer programming", "Machine learning"]),
            vec![Shelf::Programming, Shelf::DataAi],
            "the second shelf has half the first's count"
        );
        assert_eq!(
            shelves_for(&["Python", "Computer programming", "Software", "Statistics"]),
            vec![Shelf::Programming],
            "one subject in four is not a second shelf"
        );
        assert_eq!(
            shelves_for(&["Linux", "Raspberry Pi (Computer)"]),
            vec![Shelf::CloudDevOps, Shelf::Electronics],
            "a tie goes to table order"
        );
        assert_eq!(shelves_for(&["Cooking", "Gardening"]), vec![Shelf::Other]);
        assert_eq!(shelves_for::<&str>(&[]), Vec::<Shelf>::new());
    }

    #[test]
    fn catalogue_noise_is_no_subject() {
        assert_eq!(
            shelves_for(&["Accessible book", "Protected DAISY", "In library"]),
            Vec::<Shelf>::new(),
            "noise only must not put the book on Other"
        );
        assert_eq!(
            usable_subjects(&[
                "  ",
                "Electronic books",
                "Cooking",
                "cooking",
                "nyt:hardcover"
            ]),
            vec!["Cooking".to_owned()]
        );
        assert_eq!(
            shelves_for(&["Electronic books", "Accessible book"]),
            Vec::<Shelf>::new(),
            "electronic books are not Electronics"
        );
    }

    #[test]
    fn pdf_keywords_count_only_as_a_list() {
        assert_eq!(
            pdf_keyword_list("Rust; systems programming, concurrency"),
            vec!["Rust", "systems programming", "concurrency"]
        );
        assert!(
            pdf_keyword_list("Rust").is_empty(),
            "one word is not a list"
        );
        assert!(
            pdf_keyword_list(
                "This book walks you through building a web server from scratch, step by step"
            )
            .is_empty(),
            "a sentence is a blurb"
        );
    }

    #[test]
    fn only_empty_or_other_needs_a_lookup() {
        assert!(needs_lookup::<&str>(&[]));
        assert!(needs_lookup(&["Other"]));
        assert!(!needs_lookup(&["Programming"]));
        assert!(!needs_lookup(&["Other", "Web"]));
    }

    #[test]
    fn labels_and_names_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for s in Source::ALL {
            assert!(seen.insert(s.label()), "duplicate source {}", s.label());
        }
        let mut seen = std::collections::HashSet::new();
        for o in Outcome::ALL {
            assert!(seen.insert(o.label()), "duplicate outcome {}", o.label());
        }
        let mut seen = std::collections::HashSet::new();
        for s in Shelf::ALL {
            assert!(seen.insert(s.name()), "duplicate shelf {}", s.name());
        }
        assert_eq!(Shelf::ALL.last(), Some(&Shelf::Other));
    }

    #[test]
    fn every_keyword_is_already_normalised() {
        for s in Shelf::ALL {
            for kw in s.keywords() {
                assert_eq!(normalise(kw), *kw, "{} keyword {kw:?}", s.name());
            }
        }
        for n in NOISE {
            assert_eq!(normalise(n), *n);
        }
    }

    #[test]
    fn the_outcome_follows_the_shelves() {
        assert_eq!(Outcome::of_shelves(&[]), Outcome::NoSubjects);
        assert_eq!(Outcome::of_shelves(&[Shelf::Other]), Outcome::Other);
        assert_eq!(Outcome::of_shelves(&[Shelf::Web]), Outcome::Shelved);
    }

    #[test]
    fn a_decision_is_counted_with_its_labels() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record(Source::OpenLibrary, Outcome::NoMatch);
        });
        let found = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .any(|(k, _, _, v)| {
                let key = k.key();
                key.name() == "pharos_book_category_total"
                    && key
                        .labels()
                        .any(|l| l.key() == "source" && l.value() == "openlibrary")
                    && key
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == "no_match")
                    && v == DebugValue::Counter(1)
            });
        assert!(found, "pharos_book_category_total not recorded");
    }
}
