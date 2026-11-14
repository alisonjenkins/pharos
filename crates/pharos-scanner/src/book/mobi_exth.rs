//! Descriptive metadata and the cover out of a MOBI / AZW / AZW3 file, read
//! straight from the container's unencrypted parts (B1419, V1388).
//!
//! # Why this exists beside `boko`
//!
//! `boko::Book::open` refuses a DRM-protected file outright, so before this
//! module those books were indexed with a title from the file name and nothing
//! else. But Amazon DRM encrypts the TEXT records only. The PalmDB header,
//! record 0 (PalmDOC header, MOBI header, EXTH block, full name) and the image
//! records are stored in the clear, and they hold the title, author,
//! publisher, date, description, ISBN and the cover.
//!
//! This reads those and nothing else. It never touches a text record and does
//! not attempt to decrypt or circumvent anything.
//!
//! # Shape
//!
//! The parser works over `Read + Seek` and reads the 78-byte PalmDB header,
//! two record offsets, record 0 (capped) and, for the cover, one image record
//! (capped). A multi-megabyte book is never loaded. Every offset comes from
//! the file, so every slice is bounds-checked and every addition checked: a
//! hostile file yields an [`ExthError`], never a panic.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// PalmDB header up to (not including) the record list.
const PALMDB_HEADER_LEN: u64 = 78;
/// Offset of the u16 record count inside the PalmDB header.
const RECORD_COUNT_AT: usize = 76;
/// One record-list entry: u32 offset, u8 attributes, 3 bytes of unique id.
const RECORD_ENTRY_LEN: u64 = 8;

/// Record 0 read cap. EXTH plus the full name rarely pass tens of KiB; this
/// bounds a hostile file's claim without refusing a long description.
const MAX_RECORD0: u64 = 1 << 20;
/// Cover read cap. Kindle covers are well under 1 MiB; 16 MiB bounds a file
/// that claims an enormous image record.
const MAX_COVER: u64 = 16 << 20;
/// Longest string kept from any EXTH record, in bytes. A description is the
/// longest real field.
const MAX_FIELD: usize = 64 << 10;

const EXTH_AUTHOR: u32 = 100;
const EXTH_PUBLISHER: u32 = 101;
const EXTH_DESCRIPTION: u32 = 103;
const EXTH_ISBN: u32 = 104;
const EXTH_DATE: u32 = 106;
const EXTH_COVER_OFFSET: u32 = 201;
const EXTH_THUMB_OFFSET: u32 = 202;
const EXTH_UPDATED_TITLE: u32 = 503;

const ENCODING_CP1252: u32 = 1252;

/// What the unencrypted header of a Kindle file says.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExthMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub description: Option<String>,
    pub date: Option<String>,
    /// Raw EXTH 104 value; the caller filters it with the shared ISBN rule.
    pub isbn: Option<String>,
    /// PalmDOC encryption type: 0 none, 1 old mobipocket, 2 mobipocket DRM.
    pub encryption: u16,
    /// Record index of the cover image, when EXTH 201 names one.
    cover_record: Option<u64>,
    /// Record index of the thumbnail, when EXTH 202 names one.
    thumb_record: Option<u64>,
}

impl ExthMetadata {
    /// True when the header gave at least one descriptive field.
    pub fn has_descriptive(&self) -> bool {
        self.title.is_some()
            || self.author.is_some()
            || self.publisher.is_some()
            || self.description.is_some()
            || self.date.is_some()
            || self.isbn.is_some()
    }
}

/// Why the header could not be read. Every variant names the offending value.
#[derive(Debug)]
pub enum ExthError {
    /// The file could not be read or seeked.
    Io { op: &'static str, source: String },
    /// The bytes are not the structure a MOBI file has.
    Malformed { what: &'static str, value: u64 },
}

impl std::fmt::Display for ExthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExthError::Io { op, source } => write!(f, "{op}: {source}"),
            ExthError::Malformed { what, value } => write!(f, "malformed {what} ({value})"),
        }
    }
}

impl std::error::Error for ExthError {}

fn io_err(op: &'static str) -> impl FnOnce(std::io::Error) -> ExthError {
    move |e| ExthError::Io {
        op,
        source: e.to_string(),
    }
}

fn bad(what: &'static str, value: u64) -> ExthError {
    ExthError::Malformed { what, value }
}

fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([*s.first()?, *s.get(1)?]))
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([
        *s.first()?,
        *s.get(1)?,
        *s.get(2)?,
        *s.get(3)?,
    ]))
}

/// Read up to `len` bytes at `at`; a short file yields what is there.
fn read_at<R: Read + Seek>(
    r: &mut R,
    at: u64,
    len: u64,
    op: &'static str,
) -> Result<Vec<u8>, ExthError> {
    r.seek(SeekFrom::Start(at)).map_err(io_err(op))?;
    let mut out = Vec::new();
    r.by_ref()
        .take(len)
        .read_to_end(&mut out)
        .map_err(io_err(op))?;
    Ok(out)
}

/// Start offset of record `index`, from the PalmDB record list.
fn record_start<R: Read + Seek>(r: &mut R, index: u64, count: u64) -> Result<u64, ExthError> {
    if index >= count {
        return Err(bad("record index", index));
    }
    let at = index
        .checked_mul(RECORD_ENTRY_LEN)
        .and_then(|o| o.checked_add(PALMDB_HEADER_LEN))
        .ok_or_else(|| bad("record index", index))?;
    let raw = read_at(r, at, 4, "record list")?;
    be_u32(&raw, 0)
        .map(u64::from)
        .ok_or_else(|| bad("record list entry", index))
}

/// The byte range of record `index`: its start, and the next record's start
/// (or the end of the file for the last one).
fn record_range<R: Read + Seek>(
    r: &mut R,
    index: u64,
    count: u64,
    file_len: u64,
) -> Result<(u64, u64), ExthError> {
    let start = record_start(r, index, count)?;
    let next = index
        .checked_add(1)
        .ok_or_else(|| bad("record index", index))?;
    let end = if next < count {
        record_start(r, next, count)?
    } else {
        file_len
    };
    if start > end || end > file_len {
        return Err(bad("record bounds", start));
    }
    Ok((start, end))
}

fn palmdb_count<R: Read + Seek>(r: &mut R) -> Result<u64, ExthError> {
    let head = read_at(r, 0, PALMDB_HEADER_LEN, "palmdb header")?;
    if head.len() < usize::try_from(PALMDB_HEADER_LEN).unwrap_or(78) {
        return Err(bad("palmdb header length", head.len() as u64));
    }
    let count = be_u16(&head, RECORD_COUNT_AT).ok_or_else(|| bad("record count", 0))?;
    if count == 0 {
        return Err(bad("record count", 0));
    }
    Ok(u64::from(count))
}

fn file_len<R: Seek>(r: &mut R) -> Result<u64, ExthError> {
    r.seek(SeekFrom::End(0)).map_err(io_err("measure"))
}

/// Decode `bytes` per the MOBI text-encoding field. 65001 is UTF-8; 1252 is
/// CP-1252; anything else is treated as UTF-8 with replacement, which is what
/// every modern file uses.
fn decode(bytes: &[u8], encoding: u32) -> Option<String> {
    let bytes = bytes.get(..bytes.len().min(MAX_FIELD)).unwrap_or(bytes);
    let s: String = if encoding == ENCODING_CP1252 {
        bytes.iter().map(|b| cp1252(*b)).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    let s = s.replace('\0', "");
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// One CP-1252 byte as a char. 0x80..=0x9F differ from Latin-1; the five
/// undefined bytes map to U+FFFD.
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    match b {
        0x80..=0x9F => HIGH
            .get(usize::from(b.wrapping_sub(0x80)))
            .copied()
            .unwrap_or('\u{FFFD}'),
        _ => char::from(b),
    }
}

/// Parse record 0 into metadata. Pure over bytes, so tests can hit every
/// malformed shape without a file.
fn parse_record0(rec: &[u8]) -> Result<ExthMetadata, ExthError> {
    let encryption = be_u16(rec, 12).ok_or_else(|| bad("palmdoc header", rec.len() as u64))?;
    if rec.get(16..20) != Some(b"MOBI".as_slice()) {
        return Err(bad("mobi magic", rec.len() as u64));
    }
    let header_len = be_u32(rec, 20).ok_or_else(|| bad("mobi header length", 0))?;
    let encoding = be_u32(rec, 28).unwrap_or(65001);
    let mut meta = ExthMetadata {
        encryption,
        ..ExthMetadata::default()
    };

    // The full name is a fallback title; its offset and length are into
    // record 0.
    let full_name = match (be_u32(rec, 84), be_u32(rec, 88)) {
        (Some(off), Some(len)) => usize::try_from(off).ok().and_then(|off| {
            let end = off.checked_add(usize::try_from(len).ok()?)?;
            decode(rec.get(off..end)?, encoding)
        }),
        _ => None,
    };

    let first_image = be_u32(rec, 108).filter(|v| *v != u32::MAX).map(u64::from);
    let has_exth = be_u32(rec, 128).is_some_and(|f| f & 0x40 != 0);

    if has_exth {
        let start = 16usize
            .checked_add(usize::try_from(header_len).map_err(|_| bad("mobi header length", 0))?)
            .ok_or_else(|| bad("mobi header length", u64::from(header_len)))?;
        parse_exth(rec, start, encoding, first_image, &mut meta)?;
    }
    if meta.title.is_none() {
        meta.title = full_name;
    }
    Ok(meta)
}

fn parse_exth(
    rec: &[u8],
    start: usize,
    encoding: u32,
    first_image: Option<u64>,
    meta: &mut ExthMetadata,
) -> Result<(), ExthError> {
    let Some(magic_end) = start.checked_add(4) else {
        return Ok(());
    };
    if rec.get(start..magic_end) != Some(b"EXTH".as_slice()) {
        // The flag said EXTH but the block is not there: not an error worth
        // losing the full-name title over.
        return Ok(());
    }
    let count = be_u32(rec, start.saturating_add(8)).unwrap_or(0);
    let mut at = start.saturating_add(12);
    for _ in 0..count {
        let (Some(kind), Some(len)) = (be_u32(rec, at), be_u32(rec, at.saturating_add(4))) else {
            break;
        };
        // The length includes the 8-byte record header.
        let Some(body) = usize::try_from(len).ok().and_then(|l| l.checked_sub(8)) else {
            break;
        };
        let data_at = at.saturating_add(8);
        let Some(data) = data_at
            .checked_add(body)
            .and_then(|end| rec.get(data_at..end))
        else {
            break;
        };
        let text = || decode(data, encoding);
        match kind {
            EXTH_AUTHOR if meta.author.is_none() => meta.author = text(),
            EXTH_PUBLISHER if meta.publisher.is_none() => meta.publisher = text(),
            EXTH_DESCRIPTION if meta.description.is_none() => meta.description = text(),
            EXTH_ISBN if meta.isbn.is_none() => meta.isbn = text(),
            EXTH_DATE if meta.date.is_none() => meta.date = text(),
            EXTH_UPDATED_TITLE => meta.title = text().or(meta.title.take()),
            EXTH_COVER_OFFSET => meta.cover_record = image_record(first_image, data),
            EXTH_THUMB_OFFSET => meta.thumb_record = image_record(first_image, data),
            _ => {}
        }
        at = data_at.saturating_add(body);
    }
    Ok(())
}

/// Absolute record index of an image named by an EXTH offset (relative to the
/// first image record).
fn image_record(first_image: Option<u64>, data: &[u8]) -> Option<u64> {
    let off = be_u32(data, 0)?;
    first_image?.checked_add(u64::from(off))
}

/// Read the descriptive metadata of the file at `path`.
pub fn read_exth(path: &Path) -> Result<ExthMetadata, ExthError> {
    let mut f = std::fs::File::open(path).map_err(io_err("open"))?;
    read_exth_from(&mut f)
}

/// [`read_exth`] over any seekable source.
pub fn read_exth_from<R: Read + Seek>(r: &mut R) -> Result<ExthMetadata, ExthError> {
    let len = file_len(r)?;
    let count = palmdb_count(r)?;
    let (start, end) = record_range(r, 0, count, len)?;
    let size = (end - start).min(MAX_RECORD0);
    let rec = read_at(r, start, size, "record 0")?;
    parse_record0(&rec)
}

/// Read the cover image bytes of the file at `path`, when the header names one
/// that is a real image.
pub fn read_exth_cover(path: &Path) -> Result<Option<Vec<u8>>, ExthError> {
    let mut f = std::fs::File::open(path).map_err(io_err("open"))?;
    read_exth_cover_from(&mut f)
}

/// [`read_exth_cover`] over any seekable source.
pub fn read_exth_cover_from<R: Read + Seek>(r: &mut R) -> Result<Option<Vec<u8>>, ExthError> {
    let meta = read_exth_from(r)?;
    let len = file_len(r)?;
    let count = palmdb_count(r)?;
    for index in [meta.cover_record, meta.thumb_record].into_iter().flatten() {
        let Ok((start, end)) = record_range(r, index, count, len) else {
            continue;
        };
        if end.saturating_sub(start) > MAX_COVER {
            continue;
        }
        let bytes = read_at(r, start, end.saturating_sub(start), "cover record")?;
        if is_image(&bytes) {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

/// A record that starts like JPEG, PNG or GIF. A cover offset that lands on a
/// non-image record (a corrupt or off-by-one header) is reported as no cover
/// rather than serving garbage bytes (B149).
fn is_image(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8, 0xFF])
        || b.starts_with(&[0x89, b'P', b'N', b'G'])
        || b.starts_with(b"GIF8")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    pub(crate) const TINY_JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0xFF, 0xD9,
    ];

    fn exth_rec(kind: u32, data: &[u8]) -> Vec<u8> {
        let mut v = kind.to_be_bytes().to_vec();
        v.extend(((data.len() + 8) as u32).to_be_bytes());
        v.extend(data);
        v
    }

    /// A synthetic DRM-flagged AZW: PalmDB header, record 0 with PalmDOC
    /// encryption = 2, MOBI header, EXTH, full name; then a text record of
    /// ciphertext-like filler and one image record.
    pub(crate) fn drm_azw(
        encoding: u32,
        exth: &[(u32, Vec<u8>)],
        full_name: &[u8],
        cover_off: Option<u32>,
    ) -> Vec<u8> {
        let mut recs: Vec<(u32, Vec<u8>)> = exth.to_vec();
        if let Some(off) = cover_off {
            recs.push((EXTH_COVER_OFFSET, off.to_be_bytes().to_vec()));
        }
        let mut exth_body = Vec::new();
        for (k, d) in &recs {
            exth_body.extend(exth_rec(*k, d));
        }
        let mut exth_block = b"EXTH".to_vec();
        exth_block.extend(((exth_body.len() + 12) as u32).to_be_bytes());
        exth_block.extend((recs.len() as u32).to_be_bytes());
        exth_block.extend(exth_body);

        let mobi_len = 232u32;
        let mut r0 = vec![0u8; 16 + mobi_len as usize];
        r0[12..14].copy_from_slice(&2u16.to_be_bytes());
        r0[16..20].copy_from_slice(b"MOBI");
        r0[20..24].copy_from_slice(&mobi_len.to_be_bytes());
        r0[28..32].copy_from_slice(&encoding.to_be_bytes());
        let name_at = (r0.len() + exth_block.len()) as u32;
        r0[84..88].copy_from_slice(&name_at.to_be_bytes());
        r0[88..92].copy_from_slice(&(full_name.len() as u32).to_be_bytes());
        // Record 1 is the (encrypted) text, record 2 the first image.
        r0[108..112].copy_from_slice(&2u32.to_be_bytes());
        r0[128..132].copy_from_slice(&0x40u32.to_be_bytes());
        r0.extend(exth_block);
        r0.extend(full_name);

        let text = vec![0xA5u8; 64];
        let image = TINY_JPEG.to_vec();
        let n = 3u16;
        let base = 78 + 8 * u32::from(n) + 2;
        let offs = [
            base,
            base + r0.len() as u32,
            base + (r0.len() + text.len()) as u32,
        ];
        let mut f = vec![0u8; 76];
        f[60..68].copy_from_slice(b"BOOKMOBI");
        f.extend(n.to_be_bytes());
        for o in offs {
            f.extend(o.to_be_bytes());
            f.extend([0u8; 4]);
        }
        f.extend([0u8; 2]);
        f.extend(r0);
        f.extend(text);
        f.extend(image);
        f
    }

    fn standard() -> Vec<u8> {
        drm_azw(
            65001,
            &[
                (EXTH_AUTHOR, b"Test Author".to_vec()),
                (EXTH_PUBLISHER, b"Test House".to_vec()),
                (EXTH_DESCRIPTION, b"A synthetic blurb.".to_vec()),
                (EXTH_ISBN, b"9789300000002".to_vec()),
                (EXTH_DATE, b"2001-02-03T00:00:00+00:00".to_vec()),
                (EXTH_UPDATED_TITLE, b"Updated Title".to_vec()),
            ],
            b"Full Name Title",
            Some(0),
        )
    }

    #[test]
    fn a_drm_flagged_file_yields_its_unencrypted_metadata() {
        let m = read_exth_from(&mut Cursor::new(standard())).unwrap();
        assert_eq!(m.encryption, 2);
        assert_eq!(m.title.as_deref(), Some("Updated Title"));
        assert_eq!(m.author.as_deref(), Some("Test Author"));
        assert_eq!(m.publisher.as_deref(), Some("Test House"));
        assert_eq!(m.description.as_deref(), Some("A synthetic blurb."));
        assert_eq!(m.isbn.as_deref(), Some("9789300000002"));
        assert_eq!(m.date.as_deref(), Some("2001-02-03T00:00:00+00:00"));
    }

    #[test]
    fn the_full_name_is_the_title_when_exth_has_no_503() {
        let f = drm_azw(65001, &[(EXTH_AUTHOR, b"A".to_vec())], b"Full Name", None);
        let m = read_exth_from(&mut Cursor::new(f)).unwrap();
        assert_eq!(m.title.as_deref(), Some("Full Name"));
    }

    #[test]
    fn cp1252_text_is_decoded_by_hand() {
        let f = drm_azw(
            1252,
            &[(EXTH_AUTHOR, vec![b'J', 0xE9, b'r', 0x99, b'm', 0x92, b'e'])],
            b"T",
            None,
        );
        let m = read_exth_from(&mut Cursor::new(f)).unwrap();
        assert_eq!(m.author.as_deref(), Some("J\u{e9}r\u{2122}m\u{2019}e"));
    }

    #[test]
    fn utf8_text_is_kept() {
        let f = drm_azw(
            65001,
            &[(EXTH_AUTHOR, "Zo\u{eb}".as_bytes().to_vec())],
            b"T",
            None,
        );
        let m = read_exth_from(&mut Cursor::new(f)).unwrap();
        assert_eq!(m.author.as_deref(), Some("Zo\u{eb}"));
    }

    #[test]
    fn the_cover_is_the_image_record_named_by_the_offset() {
        let cover = read_exth_cover_from(&mut Cursor::new(standard())).unwrap();
        assert_eq!(cover.as_deref(), Some(TINY_JPEG));
    }

    #[test]
    fn an_offset_onto_a_non_image_record_is_no_cover() {
        // Offset -1 relative to the first image lands on the text record.
        let f = drm_azw(65001, &[], b"T", Some(u32::MAX));
        assert_eq!(read_exth_cover_from(&mut Cursor::new(f)).unwrap(), None);
        // Offset 9 is past the last record.
        let f = drm_azw(65001, &[], b"T", Some(9));
        assert_eq!(read_exth_cover_from(&mut Cursor::new(f)).unwrap(), None);
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        let good = standard();
        // Every truncation, and every single byte flipped to 0xFF.
        for n in 0..good.len() {
            let _ = read_exth_from(&mut Cursor::new(good[..n].to_vec()));
            let _ = read_exth_cover_from(&mut Cursor::new(good[..n].to_vec()));
        }
        for i in 0..good.len() {
            let mut f = good.clone();
            f[i] = 0xFF;
            let _ = read_exth_from(&mut Cursor::new(f.clone()));
            let _ = read_exth_cover_from(&mut Cursor::new(f));
        }
        assert!(read_exth_from(&mut Cursor::new(b"not a mobi".to_vec())).is_err());
        assert!(read_exth_from(&mut Cursor::new(Vec::new())).is_err());
    }
}
