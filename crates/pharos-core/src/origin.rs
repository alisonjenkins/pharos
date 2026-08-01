//! Where a [`MediaItem`](crate::MediaItem)'s bytes come from — 008.
//!
//! `MediaItem.path` is a `PathBuf` because for every item pharos has ever had,
//! it was one: the scanner walks absolute roots and stores what it found. 008
//! adds items with no file behind them at all, whose bytes are fetched from a
//! URL resolved fresh at play time.
//!
//! The stored path for those is a **stable synthetic** `ytdlp://<extractor>/<id>`
//! and never the resolved URL, for two reasons. `media_items.path` is `TEXT NOT
//! NULL UNIQUE`, so the column needs a value that does not change; and a
//! resolved URL is signed and rotates, so storing one would either collide on
//! re-resolution or churn the row on every play.
//!
//! # Why the parse is a type and not an `if`
//!
//! The dangerous call sites are not the playback path — that is inspected
//! deliberately. They are the background sweeps, which take a `&Path` out of a
//! struct field and hand it to ffmpeg or `File::open`. Nobody editing those
//! thinks about provenance, and the failure is silent: `ensure_generated_all`
//! fails on an unknown protocol, `is_generated` never becomes true, and the item
//! is retried every pass forever while holding a background-I/O permit (V134).
//!
//! So [`LocalPath`] is **unforgeable** outside this module, in the manner of
//! `BgPermit`: the only way to obtain one is to ask an item for its
//! [`Origin`] and match. A filesystem-only helper takes `LocalPath` by
//! signature, and a remote item then cannot reach it without the compiler
//! objecting. An `Origin` enum that call sites must *remember* to match on would
//! be the string test wearing a type.
//!
//! # The component-wise `starts_with` that makes this cheap
//!
//! `Path::starts_with` compares whole components, so
//! `Path::new("ytdlp://youtube/x").starts_with("/media")` is false, and the
//! store's `root_like_pattern` (`pharos-store-sqlx/src/lib.rs`), library
//! assignment and `restrict_to_parent` all classify a synthetic path correctly
//! with no special-casing. A remote item lands in its `CollectionFolder` by the
//! ordinary mechanism.
//!
//! It also means a synthetic path must NEVER be parked under a real scan root
//! (V136): `sweep_unseen` is safe only because a walk of a nonexistent root
//! errors out, leaving `walk_errors == 1` and skipping the sweep. Under a real
//! root the walk succeeds, finds none of the synthetic rows, and deletes them
//! all — beneath B98's blast-radius guard, which needs both >=100 deletions and
//! >25%.

use std::fmt;
use std::path::{Path, PathBuf};

/// URI scheme marking a path as synthetic — resolved through the remote
/// resolver rather than opened from disk.
///
/// One scheme, not one per site: the site is the `extractor`, which is yt-dlp's
/// own identifier for it (`youtube`, `vimeo`, ...). Adding a site is data, not
/// a code change.
pub const REMOTE_SCHEME: &str = "ytdlp";

const SCHEME_PREFIX: &str = "ytdlp://";

/// A path that is known to name a real filesystem location.
///
/// Unforgeable outside this module — obtain one by matching on [`Origin`].
/// Filesystem-only helpers take this by signature so a remote item cannot reach
/// them; see the module docs for why an `if` would not have held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalPath<'a>(&'a Path);

impl<'a> LocalPath<'a> {
    /// The underlying path. Deliberately a named method rather than a `Deref`:
    /// every place that steps out of the checked world should be greppable.
    pub fn as_path(self) -> &'a Path {
        self.0
    }
}

impl fmt::Display for LocalPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

/// A resolvable reference to media held somewhere else.
///
/// This is the STABLE identity — the thing that survives re-resolution — not the
/// URL. `extractor` and `id` are exactly what yt-dlp reports, so the pair round
/// trips back through the resolver to a fresh signed URL whenever the previous
/// one expires.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteRef {
    extractor: String,
    id: String,
}

/// Why a string is not a usable [`RemoteRef`].
///
/// Carries the offending value rather than a bare class, per the project's
/// "expose the cause" discipline: a rejection that does not say what it rejected
/// is another round of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteRefError {
    /// No `ytdlp://` prefix — this is an ordinary path, not a malformed ref.
    NotRemote,
    /// Prefix present but the remainder is not exactly `<extractor>/<id>`.
    Malformed { rest: String },
    /// A component was empty, which would make the synthetic path ambiguous.
    EmptyComponent { rest: String },
}

impl fmt::Display for RemoteRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRemote => write!(f, "not a {REMOTE_SCHEME} reference"),
            Self::Malformed { rest } => write!(
                f,
                "{REMOTE_SCHEME} reference must be <extractor>/<id>, got {rest:?}"
            ),
            Self::EmptyComponent { rest } => write!(
                f,
                "{REMOTE_SCHEME} reference has an empty component: {rest:?}"
            ),
        }
    }
}

impl std::error::Error for RemoteRefError {}

impl RemoteRef {
    /// Build a reference from a resolver's own identifiers.
    ///
    /// Returns `None` if either part is empty or contains `/`, which would make
    /// the synthetic path parse back to something different from what went in.
    pub fn new(extractor: impl Into<String>, id: impl Into<String>) -> Option<Self> {
        let extractor = extractor.into();
        let id = id.into();
        let usable = |s: &str| !s.is_empty() && !s.contains('/');
        (usable(&extractor) && usable(&id)).then_some(Self { extractor, id })
    }

    /// Parse a stored path string back into a reference.
    ///
    /// Note this reads the string, not the `Path` components: `PathBuf` stores
    /// the bytes verbatim, so the `//` in the scheme survives a round trip
    /// through the database and back, and a string prefix test is both exact and
    /// cheaper than walking components.
    pub fn parse(s: &str) -> Result<Self, RemoteRefError> {
        let rest = s
            .strip_prefix(SCHEME_PREFIX)
            .ok_or(RemoteRefError::NotRemote)?;
        let (extractor, id) = rest
            .split_once('/')
            .ok_or_else(|| RemoteRefError::Malformed {
                rest: rest.to_string(),
            })?;
        if id.contains('/') {
            return Err(RemoteRefError::Malformed {
                rest: rest.to_string(),
            });
        }
        Self::new(extractor, id).ok_or_else(|| RemoteRefError::EmptyComponent {
            rest: rest.to_string(),
        })
    }

    /// yt-dlp's identifier for the site (`youtube`, `vimeo`, ...).
    ///
    /// This is a metric label on `pharos_remote_resolve_total`, so one failing
    /// site is distinguishable from a broken resolver.
    pub fn extractor(&self) -> &str {
        &self.extractor
    }

    /// The site's own id for the video.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The value to store in `media_items.path`.
    ///
    /// Stable for the life of the item: everything that rotates (signed URLs,
    /// chosen format, CDN edge) is resolved at play time and never persisted
    /// here.
    pub fn to_synthetic_path(&self) -> PathBuf {
        PathBuf::from(self.to_string())
    }
}

impl fmt::Display for RemoteRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME_PREFIX}{}/{}", self.extractor, self.id)
    }
}

/// Where an item's bytes come from.
///
/// Obtained from [`MediaItem::origin`](crate::MediaItem::origin). Matching on it
/// is the only way to get a [`LocalPath`].
#[derive(Debug, Clone, PartialEq)]
pub enum Origin<'a> {
    /// An ordinary file the scanner walked.
    Local(LocalPath<'a>),
    /// Media fetched from elsewhere, resolved fresh at play time.
    Remote(RemoteRef),
}

impl<'a> Origin<'a> {
    /// Classify a stored path.
    ///
    /// A path that carries the scheme but does not parse is `Local`, and will
    /// therefore fail loudly at the filesystem rather than being silently
    /// treated as a resolvable reference. That direction is deliberate: a
    /// malformed synthetic path is a bug in whoever wrote the row, and the
    /// louder failure is the one that gets fixed.
    pub fn classify(path: &'a Path) -> Self {
        match path.to_str().map(RemoteRef::parse) {
            Some(Ok(r)) => Self::Remote(r),
            _ => Self::Local(LocalPath(path)),
        }
    }

    /// The local path, or `None` for a remote item.
    ///
    /// The shape background sweeps want: `let Some(p) = item.origin().local()
    /// else { continue };` declines an item the sweep cannot possibly serve,
    /// rather than retrying it every pass (V134).
    pub fn local(self) -> Option<LocalPath<'a>> {
        match self {
            Self::Local(p) => Some(p),
            Self::Remote(_) => None,
        }
    }

    /// The remote reference, or `None` for an ordinary file.
    pub fn remote(self) -> Option<RemoteRef> {
        match self {
            Self::Remote(r) => Some(r),
            Self::Local(_) => None,
        }
    }

    /// Whether this item's bytes live somewhere pharos must fetch them from.
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn an_ordinary_absolute_path_is_local() {
        let p = PathBuf::from("/media/Movies/Arrival (2016)/Arrival.mkv");
        let origin = Origin::classify(&p);
        assert!(!origin.is_remote());
        assert_eq!(
            origin.clone().local().map(|l| l.as_path()),
            Some(p.as_path())
        );
        assert_eq!(origin.remote(), None);
    }

    #[test]
    fn a_synthetic_path_round_trips_through_pathbuf() {
        // The whole scheme depends on `PathBuf` storing bytes verbatim: the
        // path is written to a TEXT column and read back, and if `//` were
        // normalised anywhere in that trip the item would silently become an
        // unopenable local file.
        let r = RemoteRef::new("youtube", "dQw4w9WgXcQ").expect("valid ref");
        let stored = r.to_synthetic_path();
        assert_eq!(stored.to_str(), Some("ytdlp://youtube/dQw4w9WgXcQ"));
        assert_eq!(Origin::classify(&stored).remote(), Some(r));
    }

    #[test]
    fn the_extractor_and_id_survive_the_round_trip() {
        // Not just "it parses" — the VALUES have to come back, because the pair
        // is what the resolver re-derives a fresh signed URL from.
        let r = RemoteRef::new("vimeo", "76979871").expect("valid ref");
        let back = RemoteRef::parse(&r.to_string()).expect("parses");
        assert_eq!(back.extractor(), "vimeo");
        assert_eq!(back.id(), "76979871");
    }

    #[test]
    fn a_path_that_merely_contains_the_scheme_is_local() {
        // A real file can be called anything. Only the prefix marks a synthetic
        // path, and every scanner path is absolute, so the two cannot collide.
        //
        // The exact form is asserted alongside the near misses on purpose: a
        // classifier that recognised NOTHING would satisfy the negative half by
        // construction, so on its own this would be a test a no-op passes.
        for s in [
            "/media/Movies/ytdlp://youtube/x.mkv",
            "/media/ytdlp/youtube/x.mkv",
            "ytdlp:/youtube/x",
        ] {
            let p = PathBuf::from(s);
            assert!(
                !Origin::classify(&p).is_remote(),
                "{s} must not classify as remote"
            );
        }
        let exact = PathBuf::from("ytdlp://youtube/x");
        assert!(
            Origin::classify(&exact).is_remote(),
            "the exact form must still classify as remote"
        );
    }

    #[test]
    fn a_malformed_synthetic_path_stays_local_so_it_fails_loudly() {
        // Deliberate direction: treating a malformed ref as resolvable would
        // hand garbage to the resolver on every play. Failing at the filesystem
        // names the bad row instead.
        //
        // Paired with the well-formed case for the same reason as above.
        for s in ["ytdlp://youtube", "ytdlp://", "ytdlp:///x", "ytdlp://a/b/c"] {
            let p = PathBuf::from(s);
            assert!(
                !Origin::classify(&p).is_remote(),
                "{s} must not classify as remote"
            );
        }
        let well_formed = PathBuf::from("ytdlp://a/b");
        assert_eq!(
            Origin::classify(&well_formed)
                .remote()
                .as_ref()
                .map(RemoteRef::id),
            Some("b"),
            "a well-formed ref must still parse, or the rejections above prove nothing"
        );
    }

    #[test]
    fn parse_errors_name_the_offending_value() {
        // "Expose the cause": a rejection that does not carry what it rejected
        // is another round of guessing.
        assert_eq!(
            RemoteRef::parse("/media/x.mkv"),
            Err(RemoteRefError::NotRemote)
        );
        assert_eq!(
            RemoteRef::parse("ytdlp://youtube"),
            Err(RemoteRefError::Malformed {
                rest: "youtube".to_string()
            })
        );
        assert_eq!(
            RemoteRef::parse("ytdlp:///abc"),
            Err(RemoteRefError::EmptyComponent {
                rest: "/abc".to_string()
            })
        );
        assert!(RemoteRef::parse("ytdlp://youtube")
            .unwrap_err()
            .to_string()
            .contains("youtube"));
    }

    #[test]
    fn a_component_containing_a_separator_is_refused_at_construction() {
        // Otherwise `to_synthetic_path` would produce a string that parses back
        // to a DIFFERENT ref, which is the silent-corruption shape.
        assert_eq!(RemoteRef::new("you/tube", "x"), None);
        assert_eq!(RemoteRef::new("youtube", "a/b"), None);
        assert_eq!(RemoteRef::new("", "x"), None);
        assert_eq!(RemoteRef::new("youtube", ""), None);
    }

    #[test]
    fn a_synthetic_path_is_not_under_any_real_scan_root() {
        // V136 — the store classifies items into libraries with a component-wise
        // `starts_with`/LIKE on the root. If a synthetic path could match a real
        // root, `sweep_unseen` would walk that root, not find the row, and
        // delete it. This is the assertion that pins the arrangement.
        let synthetic = RemoteRef::new("youtube", "dQw4w9WgXcQ")
            .expect("valid ref")
            .to_synthetic_path();
        for root in ["/media", "/media/Movies", "/", "/var/lib/pharos"] {
            assert!(
                !synthetic.starts_with(root),
                "synthetic path must not sit under scan root {root}"
            );
        }
        assert!(
            synthetic.is_relative(),
            "a scan root is absolute; this must not be"
        );
    }
}
