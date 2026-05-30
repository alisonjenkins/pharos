//! LIB-D4 — sidecar artwork detection as a [`MetadataProvider`].
//!
//! Scans for image files that live *beside* a media file (the Kodi /
//! Jellyfin sidecar conventions: `poster.jpg`, `fanart.jpg`, `logo.png`,
//! …) and the show-folder-level art for episodes, emitting one
//! [`ArtworkRef`] per discovered role-source so D7 can persist them into
//! the `artwork` table and D5 can serve a recorded local file before
//! falling back to ffmpeg frame-extraction.
//!
//! Pure *detection*: the provider only `stat`s candidate paths for
//! existence (no read, no copy). Running it in the scanner's parallel
//! probe phase keeps the `stat` syscalls off the async reactor (V5).
//!
//! [`PRIORITY`] sits between the NFO source (100) and the filename source
//! (10): a `<thumb>` artwork URL parsed from an NFO (when added) outranks a
//! bare sidecar file, but a discovered sidecar still wins over nothing.
//! Because artwork merges as a *union* keyed on `(role, source)`, the
//! priority only decides ordering, not which sidecar role gets dropped.

use std::path::{Path, PathBuf};

use pharos_core::{
    ArtworkRef, ArtworkRole, ArtworkSource, DomainResult, MediaKind, MetadataProvider,
    MetadataRequest, MetadataResult,
};

/// Merge priority for the sidecar source — below NFO (100), above the
/// filename heuristic (10).
pub const PRIORITY: i32 = 50;

/// Image extensions probed for each sidecar base name, in preference
/// order. A `.png` is preferred for transparency-bearing roles (logo /
/// disc / art) but every role accepts any of these — the first existing
/// file for a base name wins.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp"];

/// LIB-D4 — detects sidecar image files beside a media file (and, for
/// episodes, in the show folder). Stateless; cheap to clone. The existence
/// `stat`s run in the scanner's parallel probe phase (V5).
#[derive(Debug, Clone, Copy, Default)]
pub struct SidecarArtworkProvider;

impl SidecarArtworkProvider {
    /// Construct the provider. Stateless — `SidecarArtworkProvider` and
    /// `SidecarArtworkProvider::new()` are equivalent.
    pub fn new() -> Self {
        Self
    }
}

impl MetadataProvider for SidecarArtworkProvider {
    fn name(&self) -> &'static str {
        "sidecar"
    }

    fn priority(&self) -> i32 {
        PRIORITY
    }

    fn supports(&self, _kind: MediaKind) -> bool {
        // Sidecar art conventions exist for every kind (movie poster, album
        // cover, series fanart). The per-kind candidate set decides which
        // directories to probe.
        true
    }

    async fn fetch(&self, req: &MetadataRequest<'_>) -> DomainResult<MetadataResult> {
        // Pure detection: never returns Err. A missing directory / sidecar
        // is simply absent from the result (V6 spirit — one item's missing
        // art never aborts the scan).
        Ok(MetadataResult {
            artwork: detect_artwork(req),
            ..MetadataResult::default()
        })
    }
}

/// Detect every sidecar [`ArtworkRef`] for `req`. Probes the media file's
/// own directory first (item-level art), then — for episodes — the show
/// folder (series-level art). Within a directory, at most one ref per role
/// is emitted (the first matching base-name + extension). Across the two
/// directories the same role may appear twice; the resolver dedupes on
/// `(role, source)`, and the item-level dir is probed first so it wins
/// ordering.
pub(crate) fn detect_artwork(req: &MetadataRequest<'_>) -> Vec<ArtworkRef> {
    let mut out: Vec<ArtworkRef> = Vec::new();

    let stem = req
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    // Item-level: the directory holding the media file.
    if let Some(dir) = req.path.parent() {
        detect_in_dir(dir, stem, &mut out);
    }

    // Series-level (episodes only): the show folder carries poster.jpg /
    // fanart.jpg / banner.jpg / logo.png that apply to every episode. Skip
    // when the series folder equals the item's own directory (avoids a
    // redundant second probe of the same dir).
    if req.kind == MediaKind::Episode {
        if let Some(folder) = req.series.and_then(|s| s.series_folder.as_deref()) {
            let folder = Path::new(folder);
            let same_as_item = req.path.parent() == Some(folder);
            if !same_as_item {
                // No per-file stem at the series level (folder-wide art).
                detect_in_dir(folder, "", &mut out);
            }
        }
    }

    out
}

/// Probe `dir` for each known sidecar role, appending one [`ArtworkRef`]
/// per role that resolves to an existing file (de-duplicating roles
/// already present in `out` from an earlier, higher-precedence directory).
/// `stem` is the media file's base name (for the `<basename>-poster` /
/// `<basename>-fanart` / `<basename>-thumb` per-file conventions); pass
/// `""` for folder-wide directories with no owning file.
fn detect_in_dir(dir: &Path, stem: &str, out: &mut Vec<ArtworkRef>) {
    for (role, bases) in role_base_names(stem) {
        if out.iter().any(|a| a.role == role) {
            // A higher-precedence directory already supplied this role.
            continue;
        }
        if let Some(path) = first_existing(dir, &bases) {
            out.push(ArtworkRef {
                role,
                source: ArtworkSource::LocalFile(path),
            });
        }
    }
}

/// The ordered (role → candidate base names) table. Base names are tried
/// in order; for each, every extension in [`IMAGE_EXTS`] is probed. Roles
/// are listed Primary-first so the most-wanted art is detected first.
fn role_base_names(stem: &str) -> Vec<(ArtworkRole, Vec<String>)> {
    let mut table: Vec<(ArtworkRole, Vec<String>)> = Vec::new();

    let mut primary = vec![
        "poster".to_string(),
        "folder".to_string(),
        "cover".to_string(),
    ];
    let mut backdrop = vec!["fanart".to_string(), "backdrop".to_string()];
    let mut thumb: Vec<String> = Vec::new();
    if !stem.is_empty() {
        // Per-file conventions take precedence over the generic names.
        primary.insert(0, format!("{stem}-poster"));
        backdrop.insert(0, format!("{stem}-fanart"));
        thumb.push(format!("{stem}-thumb"));
    }
    thumb.push("thumb".to_string());

    table.push((ArtworkRole::Primary, primary));
    table.push((ArtworkRole::Backdrop, backdrop));
    table.push((ArtworkRole::Thumb, thumb));
    table.push((ArtworkRole::Banner, vec!["banner".to_string()]));
    table.push((
        ArtworkRole::Logo,
        vec!["logo".to_string(), "clearlogo".to_string()],
    ));
    table.push((ArtworkRole::Disc, vec!["disc".to_string()]));
    table.push((
        ArtworkRole::Art,
        vec!["clearart".to_string(), "art".to_string()],
    ));

    table
}

/// First `dir/<base>.<ext>` (over `bases` × [`IMAGE_EXTS`]) that exists on
/// disk, or `None`. A single `stat` per candidate; existence only (V5: no
/// read). A broken symlink / permission error counts as "absent" so one
/// odd file never aborts detection.
fn first_existing(dir: &Path, bases: &[String]) -> Option<PathBuf> {
    for base in bases {
        for ext in IMAGE_EXTS {
            let candidate = dir.join(format!("{base}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
