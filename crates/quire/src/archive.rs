//! The container port and its zip adapter.

use std::cell::RefCell;
use std::io::{Cursor, Read};

/// One entry larger than this when unpacked is refused, so a zip bomb cannot
/// take the reader's memory. Big enough for any real book's largest image.
pub const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// Read access to the files of a publication's container.
pub trait Archive {
    fn read(&self, path: &str) -> Result<Vec<u8>, ArchiveError>;
    /// Unpacked size, used to weigh sections for whole-book progress.
    fn size(&self, path: &str) -> Option<u64>;
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("not a zip archive: {0}")]
    NotZip(#[source] zip::result::ZipError),
    #[error("{path} is not in the book")]
    Missing { path: String },
    #[error("cannot open {path} in the book: {source}")]
    Entry {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("{path} unpacks to more than {MAX_ENTRY_BYTES} bytes")]
    TooLarge { path: String },
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// The zip adapter. Entries are inflated on demand, not at open, so a book
/// heavy with images opens as fast as a plain one.
pub struct ZipArchive {
    inner: RefCell<zip::ZipArchive<Cursor<Vec<u8>>>>,
}

impl ZipArchive {
    pub fn new(bytes: Vec<u8>) -> Result<Self, ArchiveError> {
        let inner = zip::ZipArchive::new(Cursor::new(bytes)).map_err(ArchiveError::NotZip)?;
        Ok(Self {
            inner: RefCell::new(inner),
        })
    }
}

impl Archive for ZipArchive {
    fn read(&self, path: &str) -> Result<Vec<u8>, ArchiveError> {
        let mut zip = self.inner.borrow_mut();
        let entry = zip.by_name(path).map_err(|e| match e {
            zip::result::ZipError::FileNotFound => ArchiveError::Missing {
                path: path.to_owned(),
            },
            source => ArchiveError::Entry {
                path: path.to_owned(),
                source,
            },
        })?;
        if entry.size() > MAX_ENTRY_BYTES {
            return Err(ArchiveError::TooLarge {
                path: path.to_owned(),
            });
        }
        let mut out = Vec::new();
        // The declared size can lie; the cap holds for what actually inflates.
        entry
            .take(MAX_ENTRY_BYTES.saturating_add(1))
            .read_to_end(&mut out)
            .map_err(|source| ArchiveError::Read {
                path: path.to_owned(),
                source,
            })?;
        if out.len() as u64 > MAX_ENTRY_BYTES {
            return Err(ArchiveError::TooLarge {
                path: path.to_owned(),
            });
        }
        Ok(out)
    }

    fn size(&self, path: &str) -> Option<u64> {
        self.inner.borrow_mut().by_name(path).ok().map(|e| e.size())
    }
}
