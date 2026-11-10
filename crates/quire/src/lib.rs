//! Open an EPUB from its bytes and read it: metadata, table of contents,
//! reading order, each section as a sanitised tree, and positions that do not
//! depend on layout.
//!
//! `quire` does no layout and touches no DOM; a renderer (such as
//! `quire-web`) builds the page from [`Section::root`]. Book content is
//! untrusted: scripts, event handlers, forms, embedded frames, `@import`,
//! `@font-face` and every URL that would load from outside the book are
//! removed before a [`Section`] is handed out.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let book = quire::Publication::open_epub(std::fs::read("book.epub")?)?;
//! let first = book.section(0)?;
//! println!("{:?}: {}", book.metadata().title, first.text());
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

mod archive;
mod css;
mod epub;
mod fixed;
mod locator;
mod path;
mod section;
mod xml;

pub use archive::{Archive, ArchiveError, ZipArchive, MAX_ENTRY_BYTES};
pub use css::Css;
pub use epub::{Metadata, OpenError, SpineItem, TocEntry};
pub use fixed::{page_progress, FixedLayout, FixedLayoutError, PageSize, Raster};
pub use locator::{char_index, utf16_offset, Locator, PageLocator, ParseLocatorError};
pub use section::{Attr, Content, El, Namespace, ParseTargetError, Section, Target, Value};

use epub::ManifestItem;
use xml::XmlError;

/// An open book.
pub struct Publication {
    archive: Box<dyn Archive>,
    metadata: Metadata,
    manifest: Vec<ManifestItem>,
    spine: Vec<SpineItem>,
    toc: Vec<TocEntry>,
    /// Unpacked size of each spine item, the weights for [`Self::progress`].
    weights: Vec<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum SectionError {
    #[error("no section {index}; the book has {len}")]
    OutOfRange { index: usize, len: usize },
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error("{path}: {source}")]
    Xml {
        path: String,
        #[source]
        source: XmlError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl Publication {
    /// Opens an EPUB 2 or 3 file held in memory.
    pub fn open_epub(bytes: Vec<u8>) -> Result<Self, OpenError> {
        Self::open_epub_from(Box::new(ZipArchive::new(bytes)?))
    }

    /// Opens an EPUB from any [`Archive`], for callers with their own
    /// container (an unpacked directory, a remote range reader).
    pub fn open_epub_from(archive: Box<dyn Archive>) -> Result<Self, OpenError> {
        let pkg = epub::open(archive.as_ref())?;
        let weights = pkg
            .spine
            .iter()
            .map(|s| archive.size(&s.path).unwrap_or(1).max(1))
            .collect();
        Ok(Self {
            archive,
            metadata: pkg.metadata,
            manifest: pkg.manifest,
            spine: pkg.spine,
            toc: pkg.toc,
            weights,
        })
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn toc(&self) -> &[TocEntry] {
        &self.toc
    }

    pub fn spine(&self) -> &[SpineItem] {
        &self.spine
    }

    /// The `index`th reading-order item, sanitised.
    pub fn section(&self, index: usize) -> Result<Section, SectionError> {
        let item = self.spine.get(index).ok_or(SectionError::OutOfRange {
            index,
            len: self.spine.len(),
        })?;
        let bytes = self.archive.read(&item.path)?;
        let doc = xml::parse_html_bytes(&bytes).map_err(|source| SectionError::Xml {
            path: item.path.clone(),
            source,
        })?;
        let spine_of = |p: &str| self.spine.iter().position(|s| s.path == p);
        let stylesheet = |p: &str| {
            self.archive
                .read(p)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        };
        Ok(section::sanitise(
            &doc,
            &section::Book {
                path: &item.path,
                index,
                spine_of: &spine_of,
                stylesheet: &stylesheet,
            },
        ))
    }

    /// A file inside the book by archive path, as a [`Section`] names it.
    ///
    /// The bytes are the book's own and are not sanitised. The media type is
    /// an image type, `text/css` or `application/octet-stream`, never one a
    /// browser would run as a document (`text/html`, `application/xhtml+xml`),
    /// whatever the manifest claims. An `image/svg+xml` resource is safe as an
    /// image source, where SVG cannot run script, and must not be opened as a
    /// document.
    pub fn resource(&self, path: &str) -> Result<Resource, ArchiveError> {
        let bytes = self.archive.read(path)?;
        let declared = self
            .manifest
            .iter()
            .find(|m| m.path == path)
            .map(|m| m.media_type.to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| guess_media_type(path).to_owned());
        let media_type = if PASSIVE_MEDIA_TYPES.contains(&declared.as_str()) {
            declared
        } else {
            "application/octet-stream".to_owned()
        };
        Ok(Resource { bytes, media_type })
    }

    /// How far through the whole book a place is, from 0 to 1. `within` is
    /// the fraction of the way through section `spine`; sections are weighed
    /// by their unpacked size, so a long chapter counts for more.
    pub fn progress(&self, spine: usize, within: f64) -> f64 {
        let total: u64 = self.weights.iter().fold(0u64, |a, w| a.saturating_add(*w));
        if total == 0 {
            return 0.0;
        }
        let before: u64 = self
            .weights
            .iter()
            .take(spine)
            .fold(0u64, |a, w| a.saturating_add(*w));
        let here = self.weights.get(spine).copied().unwrap_or(0) as f64;
        let within = if within.is_finite() {
            within.clamp(0.0, 1.0)
        } else {
            0.0
        };
        ((before as f64 + here * within) / total as f64).clamp(0.0, 1.0)
    }
}

/// Media types [`Publication::resource`] passes through.
const PASSIVE_MEDIA_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/bmp",
    "image/svg+xml",
    "text/css",
];

fn guess_media_type(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "css" => "text/css",
        "xhtml" | "html" | "htm" => "application/xhtml+xml",
        _ => "application/octet-stream",
    }
}
