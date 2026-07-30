//! comic-archive reading (004-books, US2).
//!
//! A comic is an archive of images plus, by convention, a `ComicInfo.xml`
//! written by ComicRack and every tagger since. There is no rendering here for
//! the same reason there is none for epub: jellyfin-web's `comicsPlayer`
//! downloads the file and unpacks it in the browser with libarchive.js. pharos
//! reads the archive only to answer two questions — how many pages, and what
//! does the tagger say this is.
//!
//! # The four extensions are four different containers
//!
//! `.cbz`/`.cbt`/`.cb7`/`.cbr` are zip, tar, 7z and rar with comic names. Three
//! have pure-Rust readers in the tree; rar does not, and will not:
//! `unrar` wraps a C library, the same objection that rules out a PDF
//! rasteriser (R7/R11). So a `.cbr` is deliberately **readable but cover-less**
//! — the client unpacks it fine, pharos reports no page count, and the
//! classify counter says `rar_unsupported` so that gap is a number on a
//! dashboard rather than a mystery.
//!
//! # Page count comes from the archive, never from `ComicInfo.xml`
//!
//! `<PageCount>` is written by the tagger and is wrong often enough to be
//! useless — it counts what the tagger saw, not what the file holds. The
//! image-entry count is ground truth and is what the reader will paginate.

use std::io::Read;
use std::path::Path;

use pharos_core::{BookFormat, BookMeta};
use quick_xml::events::Event;

use super::{push_entity, record_classify, ClassifyReason, ClassifyVerdict};

/// Which container an extension actually names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComicContainer {
    /// `.cbz` — zip.
    Zip,
    /// `.cbt` — tar.
    Tar,
    /// `.cb7` — 7z.
    SevenZ,
    /// `.cbr` — rar. No pure-Rust reader; permanently unread (R7).
    Rar,
}

impl ComicContainer {
    /// The container a comic path names, or `None` if the extension is not a
    /// comic at all.
    pub fn for_path(path: &Path) -> Option<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)?;
        match ext.as_str() {
            "cbz" => Some(ComicContainer::Zip),
            "cbt" => Some(ComicContainer::Tar),
            "cb7" => Some(ComicContainer::SevenZ),
            "cbr" => Some(ComicContainer::Rar),
            _ => None,
        }
    }
}

/// Everything a comic archive told us.
///
/// Wider than [`BookMeta`] for the same reason [`super::epub::EpubMetadata`] is:
/// `title`, `date` and `description` are ordinary item fields that the metadata
/// provider (T066) folds into the resolver, and parsing the archive twice to
/// serve two consumers would double the I/O on a file that lives on NFS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComicMetadata {
    /// Number of image entries. `None` only when the archive could not be read
    /// — an archive that opened and held no images reports `Some(0)`, which is
    /// a different fact and is why this is not a bare `usize`.
    pub page_count: Option<u32>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub description: Option<String>,
    /// `Year`/`Month`/`Day` joined ISO-style. Partial dates stay partial: a
    /// `Year` alone yields `"1986"`, never `"1986-01-01"`.
    pub date: Option<String>,
    pub series_name: Option<String>,
    pub series_index: Option<u32>,
    /// Archive entry path of the first image in reading order — the cover. The
    /// BYTES are read separately (T062) so metadata-only callers do not pay for
    /// a decompression.
    pub cover_entry: Option<String>,
}

impl ComicMetadata {
    /// The book-specific subset that is persisted on the item.
    pub fn to_book_meta(&self) -> BookMeta {
        BookMeta {
            format: BookFormat::Comic,
            // Unlike an epub, a comic HAS a fixed page count: one image, one
            // page, independent of any viewport.
            page_count: self.page_count,
            author: self.author.clone(),
            publisher: self.publisher.clone(),
            series_name: self.series_name.clone(),
            series_index: self.series_index,
            isbn: None,
        }
    }
}

/// Why a comic archive could not be read. Every variant carries the offending
/// value — never a bare class.
#[derive(Debug)]
pub enum ComicError {
    /// Not a comic extension at all.
    NotAComic(String),
    /// rar. Not a failure — a permanent, deliberate limit (R7).
    RarUnsupported(String),
    Open(String),
    /// The archive is malformed, truncated, or not the format its extension
    /// claims. Carries the container we tried and the reader's own message.
    Unreadable {
        container: String,
        detail: String,
    },
}

impl std::fmt::Display for ComicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComicError::NotAComic(ext) => write!(f, "extension {ext:?} is not a comic archive"),
            ComicError::RarUnsupported(name) => write!(
                f,
                "{name:?} is a rar; pharos has no pure-Rust rar reader by design, \
                 so it is downloadable and client-readable but cover-less"
            ),
            ComicError::Open(e) => write!(f, "cannot open file: {e}"),
            ComicError::Unreadable { container, detail } => {
                write!(f, "unreadable {container} archive: {detail}")
            }
        }
    }
}

/// Read a comic archive's page count and `ComicInfo.xml`.
///
/// Opens the archive exactly once. For zip and 7z the entry list comes from the
/// central directory / header, so counting pages costs no decompression at all;
/// only `ComicInfo.xml` itself is inflated, and only when it is present.
pub fn read_comic(path: &Path) -> Result<ComicMetadata, ComicError> {
    let container = ComicContainer::for_path(path).ok_or_else(|| {
        ComicError::NotAComic(
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string(),
        )
    })?;

    let (names, comic_info) = match container {
        ComicContainer::Rar => {
            return Err(ComicError::RarUnsupported(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string(),
            ))
        }
        ComicContainer::Zip => read_zip(path)?,
        ComicContainer::Tar => read_tar(path)?,
        ComicContainer::SevenZ => read_7z(path)?,
    };

    let mut pages: Vec<String> = names.into_iter().filter(|n| is_page_entry(n)).collect();
    // Reading order, not archive order: an archive lists entries in whatever
    // order the packer wrote them, and the FIRST page is the cover. Natural
    // ordering, so `page10.jpg` sorts after `page2.jpg` — plain lexicographic
    // order would make page 10 the cover of any comic that is not zero-padded.
    pages.sort_by(|a, b| natural_cmp(a, b));

    let mut meta = ComicMetadata {
        page_count: Some(pages.len() as u32),
        cover_entry: pages.first().cloned(),
        ..Default::default()
    };
    if let Some(xml) = comic_info {
        // A malformed ComicInfo.xml loses the tagger's metadata, not the book:
        // the page count above already stands on its own.
        match parse_comic_info(&xml) {
            Ok(tagged) => meta.absorb(tagged),
            Err(detail) => tracing::warn!(
                path = %path.display(),
                error = %detail,
                "ComicInfo.xml is malformed; importing with archive-derived data only"
            ),
        }
    }
    Ok(meta)
}

/// Read the cover image's BYTES — the first page in reading order.
///
/// A second open of the archive, for the same reason the epub reader takes one:
/// metadata is read on every scan pass, the cover only for a book the scan is
/// actually writing. `Ok(None)` means the archive held no images at all, which
/// is a normal outcome and not an error.
pub fn read_comic_cover(path: &Path) -> Result<Option<Vec<u8>>, ComicError> {
    let Some(entry) = read_comic(path)?.cover_entry else {
        return Ok(None);
    };
    let container = ComicContainer::for_path(path)
        .ok_or_else(|| ComicError::NotAComic(path.to_string_lossy().into_owned()))?;
    let bytes = match container {
        // Unreachable: `read_comic` above already returned `RarUnsupported`.
        ComicContainer::Rar => return Err(ComicError::RarUnsupported(entry)),
        ComicContainer::Zip => {
            let file = std::fs::File::open(path).map_err(|e| ComicError::Open(e.to_string()))?;
            let mut zip = zip::ZipArchive::new(file).map_err(|e| ComicError::Unreadable {
                container: "zip".into(),
                detail: e.to_string(),
            })?;
            let idx = (0..zip.len())
                .find(|i| {
                    zip.by_index_raw(*i)
                        .map(|e| e.name() == entry)
                        .unwrap_or(false)
                })
                .ok_or_else(|| ComicError::Unreadable {
                    container: "zip".into(),
                    detail: format!("cover entry {entry:?} vanished between passes"),
                })?;
            let mut e = zip.by_index(idx).map_err(|e| ComicError::Unreadable {
                container: "zip".into(),
                detail: e.to_string(),
            })?;
            let mut out = Vec::new();
            e.read_to_end(&mut out)
                .map_err(|e| ComicError::Unreadable {
                    container: "zip".into(),
                    detail: e.to_string(),
                })?;
            out
        }
        ComicContainer::Tar => {
            let file = std::fs::File::open(path).map_err(|e| ComicError::Open(e.to_string()))?;
            let mut archive = tar::Archive::new(file);
            let entries = archive.entries().map_err(|e| ComicError::Unreadable {
                container: "tar".into(),
                detail: e.to_string(),
            })?;
            let mut found = None;
            for e in entries {
                let mut e = e.map_err(|e| ComicError::Unreadable {
                    container: "tar".into(),
                    detail: e.to_string(),
                })?;
                let is_cover = e
                    .path()
                    .map(|p| p.to_string_lossy() == entry)
                    .unwrap_or(false);
                if is_cover {
                    let mut out = Vec::new();
                    e.read_to_end(&mut out)
                        .map_err(|e| ComicError::Unreadable {
                            container: "tar".into(),
                            detail: e.to_string(),
                        })?;
                    found = Some(out);
                    break;
                }
            }
            found.ok_or_else(|| ComicError::Unreadable {
                container: "tar".into(),
                detail: format!("cover entry {entry:?} vanished between passes"),
            })?
        }
        ComicContainer::SevenZ => {
            let mut file =
                std::fs::File::open(path).map_err(|e| ComicError::Open(e.to_string()))?;
            let len = file
                .metadata()
                .map_err(|e| ComicError::Open(e.to_string()))?
                .len();
            let archive = sevenz_rust::Archive::read(&mut file, len, &[]).map_err(|e| {
                ComicError::Unreadable {
                    container: "7z".into(),
                    detail: e.to_string(),
                }
            })?;
            let mut reader = sevenz_rust::SevenZReader::from_archive(
                archive,
                file,
                sevenz_rust::Password::empty(),
            );
            let mut found = None;
            reader
                .for_each_entries(|e, rd| {
                    if found.is_none() && e.name == entry {
                        let mut out = Vec::new();
                        if rd.read_to_end(&mut out).is_ok() {
                            found = Some(out);
                        }
                    }
                    Ok(true)
                })
                .map_err(|e| ComicError::Unreadable {
                    container: "7z".into(),
                    detail: e.to_string(),
                })?;
            found.ok_or_else(|| ComicError::Unreadable {
                container: "7z".into(),
                detail: format!("cover entry {entry:?} vanished between passes"),
            })?
        }
    };
    Ok(Some(bytes))
}

/// The scan path's decision: a parse failure yields a page-count-less
/// `BookMeta` rather than dropping the item (V6), and records WHY.
///
/// Records only the PARSE outcome — except for `.cbr`, whose verdict IS a cover
/// verdict and can be settled here because it is settled for every rar there
/// will ever be. Everything else's cover verdict belongs to the extractor,
/// which is the only step that knows whether bytes came out.
pub fn read_comic_or_empty(path: &Path) -> BookMeta {
    match read_comic(path) {
        Ok(meta) => meta.to_book_meta(),
        Err(err) => {
            let (verdict, reason) = match &err {
                // NOT `Unparseable`: a `.cbr` parses fine in the client. The
                // verdict is about the COVER, and the reason names the design
                // limit rather than implying a fault.
                ComicError::RarUnsupported(_) => {
                    (ClassifyVerdict::CoverAbsent, ClassifyReason::RarUnsupported)
                }
                ComicError::Open(_) => (ClassifyVerdict::Unparseable, ClassifyReason::Unopenable),
                _ => (
                    ClassifyVerdict::Unparseable,
                    ClassifyReason::MalformedContainer,
                ),
            };
            record_classify(BookFormat::Comic, verdict, reason);
            // rar is expected and permanent, so it is not a warning; the other
            // cases are something going wrong with a specific file.
            if matches!(err, ComicError::RarUnsupported(_)) {
                tracing::debug!(path = %path.display(), reason = %err, "comic: no cover by design");
            } else {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "comic archive unreadable; importing with filename title only"
                );
            }
            BookMeta {
                format: BookFormat::Comic,
                ..Default::default()
            }
        }
    }
}

impl ComicMetadata {
    /// Fold tagger metadata in without letting it overwrite the archive-derived
    /// page count or cover.
    fn absorb(&mut self, tagged: ComicMetadata) {
        self.title = tagged.title;
        self.author = tagged.author;
        self.publisher = tagged.publisher;
        self.description = tagged.description;
        self.date = tagged.date;
        self.series_name = tagged.series_name;
        self.series_index = tagged.series_index;
    }
}

/// The conventional filename, matched case-insensitively and anywhere in the
/// archive (some taggers nest it beside the pages).
fn is_comic_info(name: &str) -> bool {
    name.rsplit('/')
        .next()
        .is_some_and(|f| f.eq_ignore_ascii_case("ComicInfo.xml"))
}

/// Entry names plus `ComicInfo.xml`'s body, from a zip.
fn read_zip(path: &Path) -> Result<(Vec<String>, Option<String>), ComicError> {
    let file = std::fs::File::open(path).map_err(|e| ComicError::Open(e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| ComicError::Unreadable {
        container: "zip".into(),
        detail: e.to_string(),
    })?;

    // Names come from the central directory — no decompression.
    let mut names = Vec::with_capacity(zip.len());
    let mut info_idx = None;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index_raw(i) else {
            continue;
        };
        let name = entry.name().to_string();
        if info_idx.is_none() && is_comic_info(&name) {
            info_idx = Some(i);
        }
        names.push(name);
    }

    // Only ComicInfo.xml is inflated, and only when it exists.
    let info = info_idx.and_then(|i| {
        let mut entry = zip.by_index(i).ok()?;
        let mut out = String::new();
        entry.read_to_string(&mut out).ok()?;
        Some(out)
    });
    Ok((names, info))
}

/// Entry names plus `ComicInfo.xml`'s body, from a tar.
///
/// A tar has no index, so this is one sequential pass: names are collected as
/// they stream by and `ComicInfo.xml` is read at the moment it appears. There
/// is no second pass to make.
fn read_tar(path: &Path) -> Result<(Vec<String>, Option<String>), ComicError> {
    let file = std::fs::File::open(path).map_err(|e| ComicError::Open(e.to_string()))?;
    let mut archive = tar::Archive::new(file);
    let entries = archive.entries().map_err(|e| ComicError::Unreadable {
        container: "tar".into(),
        detail: e.to_string(),
    })?;

    let mut names = Vec::new();
    let mut info = None;
    for entry in entries {
        let mut entry = entry.map_err(|e| ComicError::Unreadable {
            container: "tar".into(),
            detail: e.to_string(),
        })?;
        let Ok(p) = entry.path() else { continue };
        let name = p.to_string_lossy().into_owned();
        if info.is_none() && is_comic_info(&name) {
            let mut out = String::new();
            if entry.read_to_string(&mut out).is_ok() {
                info = Some(out);
            }
        }
        names.push(name);
    }
    Ok((names, info))
}

/// Entry names plus `ComicInfo.xml`'s body, from a 7z.
///
/// The header is parsed first, which is enough to count pages without
/// decompressing anything. The decoder only runs when `ComicInfo.xml` is
/// present, and it reuses the already-open handle and already-parsed header, so
/// the file is still opened exactly once.
fn read_7z(path: &Path) -> Result<(Vec<String>, Option<String>), ComicError> {
    let mut file = std::fs::File::open(path).map_err(|e| ComicError::Open(e.to_string()))?;
    let len = file
        .metadata()
        .map_err(|e| ComicError::Open(e.to_string()))?
        .len();
    let archive =
        sevenz_rust::Archive::read(&mut file, len, &[]).map_err(|e| ComicError::Unreadable {
            container: "7z".into(),
            detail: e.to_string(),
        })?;

    let names: Vec<String> = archive.files.iter().map(|f| f.name.clone()).collect();
    if !names.iter().any(|n| is_comic_info(n)) {
        return Ok((names, None));
    }

    let mut reader =
        sevenz_rust::SevenZReader::from_archive(archive, file, sevenz_rust::Password::empty());
    let mut info = None;
    reader
        .for_each_entries(|entry, rd| {
            if info.is_none() && is_comic_info(&entry.name) {
                let mut out = String::new();
                if rd.read_to_string(&mut out).is_ok() {
                    info = Some(out);
                }
            }
            Ok(true)
        })
        .map_err(|e| ComicError::Unreadable {
            container: "7z".into(),
            detail: e.to_string(),
        })?;
    Ok((names, info))
}

/// Image extensions a comic reader will paginate. Deliberately a fixed list:
/// counting every non-directory entry would make `ComicInfo.xml` and a
/// `Thumbs.db` into pages.
const PAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "avif", "tif", "tiff",
];

/// True when an archive entry is a page.
fn is_page_entry(name: &str) -> bool {
    // Directories are entries too, in both zip and tar.
    if name.ends_with('/') {
        return false;
    }
    // macOS zips carry a shadow `__MACOSX/` tree of `._name` resource forks
    // that have image extensions and are not pages. Counting them doubles the
    // page count and makes a resource fork the cover.
    if name
        .split('/')
        .any(|seg| seg == "__MACOSX" || seg.starts_with("._") || seg == ".DS_Store")
    {
        return false;
    }
    let Some(file) = name.rsplit('/').next() else {
        return false;
    };
    if file.starts_with('.') {
        return false;
    }
    file.rsplit_once('.').is_some_and(|(_, ext)| {
        let ext = ext.to_ascii_lowercase();
        PAGE_EXTENSIONS.contains(&ext.as_str())
    })
}

/// Compare two entry names the way a reader orders pages: digit runs compare as
/// numbers, everything else byte-wise and case-insensitively.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut ai, mut bi) = (a.bytes().peekable(), b.bytes().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let xn = take_number(&mut ai);
                let yn = take_number(&mut bi);
                match xn.cmp(&yn) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(x), Some(y)) => {
                ai.next();
                bi.next();
                match x
                    .to_ascii_lowercase()
                    .cmp(&y.to_ascii_lowercase())
                    .then(x.cmp(&y))
                {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
        }
    }
}

/// Consume a run of digits as one number. Saturates rather than wrapping: a
/// 40-digit run in a filename is nonsense, and ordering it consistently matters
/// more than ordering it correctly.
fn take_number(it: &mut std::iter::Peekable<std::str::Bytes<'_>>) -> u128 {
    let mut n: u128 = 0;
    while let Some(d) = it.peek().copied() {
        if !d.is_ascii_digit() {
            break;
        }
        it.next();
        n = n.saturating_mul(10).saturating_add(u128::from(d - b'0'));
    }
    n
}

/// Parse `ComicInfo.xml`.
///
/// The same streaming style as the epub OPF reader, and for the same reason:
/// the schema is flat, taggers emit fields in no fixed order and add their own,
/// and quick-xml 0.41 delivers entities as separate events so a `&amp;` in a
/// title arrives in pieces.
fn parse_comic_info(xml: &str) -> Result<ComicMetadata, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    // NOT `trim_text(true)`. Trimming happens per literal RUN, and an entity
    // splits a run — so `Bruce Wayne &amp; the Mutants` would lose the spaces
    // either side of the `&` and come back "Bruce Wayne&the Mutants". The
    // element's text is trimmed once, whole, at its `End`.
    reader.config_mut().trim_text(false);

    let mut out = ComicMetadata::default();
    let (mut year, mut month, mut day) = (None, None, None);
    let mut current: Option<Vec<u8>> = None;
    let mut text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current = Some(local_name(e.name().as_ref()).to_ascii_lowercase());
                text.clear();
            }
            Ok(Event::Text(t)) if current.is_some() => {
                if let Ok(decoded) = t.decode() {
                    text.push_str(&decoded);
                }
            }
            Ok(Event::GeneralRef(r)) if current.is_some() => push_entity(&r, &mut text),
            Ok(Event::End(_)) => {
                if let Some(field) = current.take() {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        assign_field(&field, trimmed, &mut out, &mut year, &mut month, &mut day);
                    }
                }
                text.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }

    // A partial date stays partial. Fabricating "1986-01-01" from a bare year
    // would put a wrong day on every comic that only tagged a year.
    out.date = match (year, month, day) {
        (Some(y), Some(m), Some(d)) => Some(format!("{y:04}-{m:02}-{d:02}")),
        (Some(y), Some(m), None) => Some(format!("{y:04}-{m:02}")),
        (Some(y), None, _) => Some(format!("{y:04}")),
        _ => None,
    };
    Ok(out)
}

fn assign_field(
    field: &[u8],
    text: String,
    out: &mut ComicMetadata,
    year: &mut Option<u32>,
    month: &mut Option<u32>,
    day: &mut Option<u32>,
) {
    match field {
        b"title" => out.title = Some(text),
        b"series" => out.series_name = Some(text),
        // "1", "1.5" and "001" all appear. Take the integer part, as the epub
        // reader does with calibre's "1.0".
        b"number" => {
            out.series_index = text
                .split('.')
                .next()
                .and_then(|n| n.trim().parse::<u32>().ok());
        }
        // ComicInfo has no author field; `Writer` is the closest and is what
        // every reader displays as the byline.
        b"writer" => out.author = Some(text),
        b"publisher" => out.publisher = Some(text),
        b"summary" => out.description = Some(text),
        b"year" => *year = text.trim().parse().ok(),
        b"month" => *month = text.trim().parse().ok(),
        b"day" => *day = text.trim().parse().ok(),
        // `PageCount` is deliberately ignored — see the module header.
        _ => {}
    }
}

/// Strip a namespace prefix, same as the OPF reader.
fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().rposition(|b| *b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;

    const COMIC_INFO: &str = r#"<?xml version="1.0"?>
<ComicInfo>
  <Title>The Dark Knight Returns</Title>
  <Series>Batman</Series>
  <Number>1</Number>
  <Writer>Frank Miller</Writer>
  <Publisher>DC Comics</Publisher>
  <Summary>Bruce Wayne &amp; the Mutants.</Summary>
  <Year>1986</Year>
  <Month>2</Month>
  <Day>1</Day>
  <PageCount>999</PageCount>
</ComicInfo>"#;

    /// The two-pixel JPEG the fixtures use as a page. Real bytes, so the
    /// extension is not the only thing making it a page.
    fn jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend_from_slice(b"\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00");
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    /// Build a `.cbz` in-test rather than checking a binary blob into the repo,
    /// so the fixture's contents are visible in the test that reads them.
    fn write_cbz(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        for (name, body) in entries {
            zw.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(body).unwrap();
        }
        zw.finish().unwrap();
    }

    fn write_cbt(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut builder = tar::Builder::new(f);
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, &body[..]).unwrap();
        }
        builder.finish().unwrap();
    }

    fn write_cb7(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let dir = path.with_extension("staging");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in entries {
            let p = dir.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        sevenz_rust::compress_to_path(&dir, path).unwrap();
    }

    /// T052 — page count is the image-entry count, and `ComicInfo.xml` maps
    /// onto the metadata fields.
    #[test]
    fn a_comics_page_count_and_comicinfo_are_read() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Batman 001.cbz");
        write_cbz(
            &p,
            &[
                ("ComicInfo.xml", COMIC_INFO.as_bytes().to_vec()),
                ("page01.jpg", jpeg()),
                ("page02.jpg", jpeg()),
            ],
        );

        let meta = read_comic(&p).expect("a well-formed cbz must parse");
        assert_eq!(
            meta.page_count,
            Some(2),
            "two images means two pages; ComicInfo's <PageCount>999</PageCount> \
             must NOT win — it is the tagger's guess, the archive is ground truth"
        );
        assert_eq!(meta.series_name.as_deref(), Some("Batman"));
        assert_eq!(meta.series_index, Some(1));
        assert_eq!(
            meta.author.as_deref(),
            Some("Frank Miller"),
            "ComicInfo has no author field; Writer is the byline"
        );
        assert_eq!(meta.title.as_deref(), Some("The Dark Knight Returns"));
        assert_eq!(meta.publisher.as_deref(), Some("DC Comics"));
        assert_eq!(
            meta.description.as_deref(),
            Some("Bruce Wayne & the Mutants."),
            "quick-xml delivers `&amp;` as a separate event; a truncated summary \
             here means the entity run was dropped"
        );
        assert_eq!(meta.date.as_deref(), Some("1986-02-01"));
        assert_eq!(
            meta.cover_entry.as_deref(),
            Some("page01.jpg"),
            "the cover is the first page in reading order, never ComicInfo.xml"
        );

        let bm = meta.to_book_meta();
        assert_eq!(bm.format, BookFormat::Comic);
        assert_eq!(
            bm.page_count,
            Some(2),
            "unlike an epub, a comic's page count is viewport-independent"
        );
        assert_eq!(bm.series_name.as_deref(), Some("Batman"));
    }

    /// T053 — the archive-only path.
    #[test]
    fn a_comic_without_comicinfo_still_reports_a_page_count() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Untagged.cbz");
        write_cbz(
            &p,
            &[
                ("001.png", jpeg()),
                ("002.png", jpeg()),
                ("003.png", jpeg()),
            ],
        );

        let meta = read_comic(&p).expect("an untagged cbz is still a comic");
        assert_eq!(meta.page_count, Some(3));
        assert_eq!(meta.cover_entry.as_deref(), Some("001.png"));
        assert_eq!(meta.series_name, None);
        assert_eq!(meta.series_index, None);
        assert_eq!(meta.author, None);
        assert_eq!(meta.title, None);
        assert_eq!(meta.date, None);
    }

    /// A partial date must stay partial — an invented month and day would be
    /// displayed as fact.
    #[test]
    fn a_year_only_comicinfo_yields_a_year_only_date() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("YearOnly.cbz");
        write_cbz(
            &p,
            &[
                (
                    "ComicInfo.xml",
                    br#"<ComicInfo><Series>X</Series><Year>1986</Year></ComicInfo>"#.to_vec(),
                ),
                ("01.jpg", jpeg()),
            ],
        );
        let meta = read_comic(&p).unwrap();
        assert_eq!(meta.date.as_deref(), Some("1986"));
    }

    /// The three readable containers must agree. Without this, a `.cbt` or
    /// `.cb7` could silently report zero pages and nothing would notice.
    #[test]
    fn every_readable_container_yields_the_same_answer() {
        let td = tempfile::tempdir().unwrap();
        let entries = vec![
            ("ComicInfo.xml", COMIC_INFO.as_bytes().to_vec()),
            ("page01.jpg", jpeg()),
            ("page02.jpg", jpeg()),
        ];

        let cbz = td.path().join("A.cbz");
        write_cbz(&cbz, &entries);
        let cbt = td.path().join("A.cbt");
        write_cbt(&cbt, &entries);
        let cb7 = td.path().join("A.cb7");
        write_cb7(&cb7, &entries);

        for p in [&cbz, &cbt, &cb7] {
            let meta = read_comic(p).unwrap_or_else(|e| panic!("{} must parse: {e}", p.display()));
            assert_eq!(
                meta.page_count,
                Some(2),
                "{} disagreed on the page count",
                p.display()
            );
            assert_eq!(
                meta.series_name.as_deref(),
                Some("Batman"),
                "{} did not read ComicInfo.xml",
                p.display()
            );
            assert_eq!(
                meta.cover_entry.as_deref(),
                Some("page01.jpg"),
                "{} picked the wrong cover",
                p.display()
            );
        }
    }

    /// T056 — `.cbr` is readable by the client and cover-less in pharos, on
    /// purpose, and the counter says so.
    #[test]
    fn a_cbr_is_readable_but_permanently_coverless() {
        use metrics_util::debugging::DebuggingRecorder;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Watchmen 01.cbr");
        std::fs::write(&p, b"Rar!\x1a\x07\x00").unwrap();

        let err = read_comic(&p).expect_err("pharos has no rar reader");
        assert!(
            matches!(err, ComicError::RarUnsupported(_)),
            "got {err:?}, which would misreport a design limit as a fault"
        );
        assert!(
            err.to_string().contains("Watchmen 01.cbr"),
            "the reason must name the file, got: {err}"
        );

        let bm = read_comic_or_empty(&p);
        assert_eq!(bm.format, BookFormat::Comic);
        assert!(
            bm.format.readable_by_client(),
            "libarchive.js unpacks rar in the browser, so the client CAN read it"
        );
        assert_eq!(
            bm.page_count, None,
            "pharos cannot count pages it cannot list — and must not invent one"
        );

        let labels = classify_labels(&snapshotter).expect("the decision must be visible");
        assert_eq!(labels.get("format").map(String::as_str), Some("comic"));
        assert_eq!(
            labels.get("verdict").map(String::as_str),
            Some("cover_absent"),
            "a cbr is not unparseable — the client parses it fine"
        );
        assert_eq!(
            labels.get("reason").map(String::as_str),
            Some("rar_unsupported"),
            "the gap must be a number on a dashboard, not a mystery"
        );
    }

    #[test]
    fn a_malformed_comic_still_imports_the_item() {
        let td = tempfile::tempdir().unwrap();

        let p = td.path().join("Lying.cbz");
        std::fs::write(&p, b"this is plain text").unwrap();
        let err = read_comic(&p).expect_err("not a zip");
        assert!(
            err.to_string().contains("zip"),
            "the error must name the container it tried, got: {err}"
        );
        assert_eq!(
            read_comic_or_empty(&p).format,
            BookFormat::Comic,
            "V6 — the item still imports"
        );

        let p = td.path().join("Gone.cb7");
        assert!(matches!(read_comic(&p), Err(ComicError::Open(_))));
        assert_eq!(read_comic_or_empty(&p).format, BookFormat::Comic);
    }

    /// An archive that opens and holds no images is a DIFFERENT fact from one
    /// that could not be opened: zero pages, not no answer.
    #[test]
    fn an_archive_with_no_images_reports_zero_pages_not_no_pages() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Empty.cbz");
        write_cbz(&p, &[("readme.txt", b"nothing here".to_vec())]);

        let meta = read_comic(&p).unwrap();
        assert_eq!(meta.page_count, Some(0));
        assert_eq!(meta.cover_entry, None);
    }

    #[test]
    fn junk_entries_are_not_pages() {
        // macOS resource forks carry image extensions and are not pages;
        // counting them doubles every page count and makes `._page01.jpg` the
        // cover, because `.` sorts before a digit.
        assert!(!is_page_entry("__MACOSX/._page01.jpg"));
        assert!(!is_page_entry("._page01.jpg"));
        assert!(!is_page_entry(".DS_Store"));
        assert!(!is_page_entry("pages/"));
        assert!(!is_page_entry("ComicInfo.xml"));
        assert!(!is_page_entry("Thumbs.db"));
        assert!(is_page_entry("pages/page01.jpg"));
        assert!(
            is_page_entry("PAGE01.JPEG"),
            "extensions are case-insensitive"
        );
        assert!(is_page_entry("01.webp"));
    }

    /// Plain lexicographic order makes page 10 the cover of any comic that is
    /// not zero-padded, and that is a common enough packing to matter.
    #[test]
    fn pages_order_naturally_not_lexicographically() {
        let mut names = vec![
            "page10.jpg".to_string(),
            "page2.jpg".to_string(),
            "page1.jpg".to_string(),
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["page1.jpg", "page2.jpg", "page10.jpg"]);

        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("Unpadded.cbz");
        write_cbz(
            &p,
            &[
                ("page10.jpg", jpeg()),
                ("page2.jpg", jpeg()),
                ("page1.jpg", jpeg()),
            ],
        );
        assert_eq!(
            read_comic(&p).unwrap().cover_entry.as_deref(),
            Some("page1.jpg"),
            "lexicographic order would have made page10 the cover"
        );
    }

    #[test]
    fn extensions_map_to_the_container_they_actually_are() {
        for (ext, want) in [
            ("cbz", ComicContainer::Zip),
            ("cbt", ComicContainer::Tar),
            ("cb7", ComicContainer::SevenZ),
            ("cbr", ComicContainer::Rar),
            ("CBZ", ComicContainer::Zip),
        ] {
            let p = std::path::PathBuf::from(format!("/c/x.{ext}"));
            assert_eq!(ComicContainer::for_path(&p), Some(want), "{ext}");
        }
        assert_eq!(
            ComicContainer::for_path(std::path::Path::new("/c/x.epub")),
            None
        );
    }

    fn classify_labels(
        snapshotter: &metrics_util::debugging::Snapshotter,
    ) -> Option<std::collections::HashMap<String, String>> {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(k, _, _, _)| {
                let key = k.key();
                (key.name() == "pharos_book_classify_total").then(|| {
                    key.labels()
                        .map(|l| (l.key().to_string(), l.value().to_string()))
                        .collect()
                })
            })
    }
}
