//! A PDF handed to an e-reader as pictures of its pages (V1460, ADR-0144).
//!
//! A Kobo has no PDF engine, and running one there would cost the memory and
//! the page-turn time it does not have. So a request for
//! `/Items/{id}/Download?pages=pictures&maxWidth=..&maxHeight=..` on a PDF is
//! answered with a CBZ: one stored picture per page, each drawn to fit the
//! box. A page with no colour on it is a 16-grey PNG (what an e-ink panel
//! shows) and one with colour is a JPEG, unless the client asks for grey
//! throughout with `grey=true`.
//!
//! The copy is built once into the book cache directory, keyed by the source
//! file's mtime and size, the box and the colour choice, so an edited book
//! gets a new copy and a repeat request costs a `stat`. Every request lands on
//! `pharos_book_pages_total{outcome}` and in one `book pages:` log line that
//! carries the inputs and the reason.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use image::codecs::jpeg::JpegEncoder;
use pharos_core::MediaId;
use quire::FixedLayout;
use tokio::sync::Semaphore;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::api::jellyfin::ci_query;
use crate::book_convert::ConvertedBookCache;
use crate::book_slim::SlimBox;

/// The box pages are drawn into when the request names none: a Kobo Libra
/// Colour's panel, the reader this was built for.
pub const DEFAULT_BOX: SlimBox = SlimBox {
    width: 1264,
    height: 1680,
};

/// Largest PDF that is drawn. The engine holds the whole file in memory.
const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;

/// Most pages a copy holds; a reader's archive directory is bounded too.
pub const MAX_PAGES: usize = 4000;

/// Largest edge a page is drawn at, so one page's pixels stay bounded
/// whatever box is asked for. A Kobo decodes at most about 6 megapixels.
const MAX_PAGE_EDGE: u32 = 3000;

const JPEG_QUALITY: u8 = 80;

/// The greys an e-ink panel shows; grey pages are stored at this depth.
const GREY_LEVELS: u8 = 16;

/// A channel spread above this makes a pixel coloured rather than grey.
const COLOUR_SPREAD: u8 = 24;

/// A page with more coloured pixels than one in this many is a colour page.
/// Anti-aliasing leaves a few faint fringes on a black-and-white page.
const COLOUR_SHARE: usize = 1000;

/// Pages are drawn on blocking threads, one book at a time: the work is all
/// CPU and a second book only slows the first.
const DRAW_SLOTS: usize = 1;

/// How long a request waits for the slot. Long, because the book drawing
/// ahead of it may well be the same one, and its copy then answers this too.
const DRAW_QUEUE_TIMEOUT: Duration = Duration::from_secs(120);

static SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(DRAW_SLOTS)));

/// What a request asks for: pictures of the pages, how big, and in colour or
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagesRequest {
    pub bx: SlimBox,
    pub grey: bool,
}

impl PagesRequest {
    /// The request `qs` makes, or `None` when it does not ask for
    /// `pages=pictures` (matched case-insensitively).
    pub fn from_query(qs: &str) -> Option<Self> {
        let wanted = ci_query::values(qs, "pages").any(|v| v.eq_ignore_ascii_case("pictures"));
        if !wanted {
            return None;
        }
        let grey = ci_query::values(qs, "grey").any(|v| v.eq_ignore_ascii_case("true") || v == "1");
        Some(Self {
            bx: SlimBox::from_query(qs).unwrap_or(DEFAULT_BOX),
            grey,
        })
    }

    fn cache_tag(self) -> String {
        format!(
            "{}x{}-{}",
            self.bx.width,
            self.bx.height,
            if self.grey { "grey" } else { "colour" }
        )
    }
}

/// What a request for pictures of a PDF's pages ended in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagesOutcome {
    /// The pages were drawn and the copy cached, and is what is served.
    Drawn,
    /// A copy drawn earlier is served from the cache.
    CacheHit,
    /// No book cache directory is configured, so there is nowhere to draw to.
    NoCache,
    /// The slot stayed busy past the wait; the client may ask again.
    Busy,
    /// The PDF could not be drawn.
    Failed,
}

impl PagesOutcome {
    pub const ALL: [PagesOutcome; 5] = [
        PagesOutcome::Drawn,
        PagesOutcome::CacheHit,
        PagesOutcome::NoCache,
        PagesOutcome::Busy,
        PagesOutcome::Failed,
    ];

    /// The `outcome` label value. A dashboard contract: do not rename.
    pub fn label(self) -> &'static str {
        match self {
            PagesOutcome::Drawn => "drawn",
            PagesOutcome::CacheHit => "cache_hit",
            PagesOutcome::NoCache => "no_cache",
            PagesOutcome::Busy => "busy",
            PagesOutcome::Failed => "failed",
        }
    }
}

/// Count one decision.
pub fn record_pages(outcome: PagesOutcome) {
    metrics::counter!("pharos_book_pages_total", "outcome" => outcome.label()).increment(1);
}

/// Why a PDF's pages could not be drawn.
#[derive(Debug, thiserror::Error)]
pub enum PagesError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the PDF is {bytes} bytes; at most {MAX_SOURCE_BYTES} are drawn")]
    TooLarge { bytes: u64 },
    #[error("open the PDF: {0}")]
    Open(#[from] quire_pdf::OpenError),
    #[error("the PDF has {pages} pages; at most {MAX_PAGES} are drawn")]
    TooManyPages { pages: usize },
    #[error("every one of the {pages} pages failed to draw; the first: {first}")]
    NothingDrawn { pages: usize, first: String },
    #[error("write the page book {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("draw task: {0}")]
    Task(String),
    #[error("the drawing slot stayed busy for {DRAW_QUEUE_TIMEOUT:?}")]
    Busy,
    #[error("no book cache directory is configured")]
    NoCache,
    #[error("the source changed while its pages were drawn")]
    Changed,
}

impl PagesError {
    fn outcome(&self) -> PagesOutcome {
        match self {
            PagesError::Busy => PagesOutcome::Busy,
            PagesError::NoCache => PagesOutcome::NoCache,
            _ => PagesOutcome::Failed,
        }
    }
}

/// What one drawing did, for the log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrawStats {
    pub pages: u32,
    pub colour: u32,
    pub grey: u32,
    pub failed: u32,
}

/// The size a `width` x `height` page is drawn at to fit `bx`, never more
/// than [`MAX_PAGE_EDGE`] on a side and never 0.
fn fitted(width: f32, height: f32, bx: SlimBox) -> (u32, u32) {
    let (width, height) = (width.max(1.0), height.max(1.0));
    let edge = MAX_PAGE_EDGE as f32;
    let scale = (bx.width.min(MAX_PAGE_EDGE) as f32 / width)
        .min(bx.height.min(MAX_PAGE_EDGE) as f32 / height)
        .min(edge / width)
        .min(edge / height);
    let side = |v: f32| (v * scale).round().clamp(1.0, edge) as u32;
    (side(width), side(height))
}

/// Whether more than one pixel in [`COLOUR_SHARE`] has colour in it.
fn has_colour(rgba: &[u8]) -> bool {
    let pixels = rgba.len() / 4;
    let allowed = pixels / COLOUR_SHARE;
    let coloured = rgba
        .chunks_exact(4)
        .filter(|p| {
            let (r, g, b) = (p[0], p[1], p[2]);
            r.max(g).max(b) - r.min(g).min(b) > COLOUR_SPREAD
        })
        .take(allowed.saturating_add(1))
        .count();
    coloured > allowed
}

/// `rgba` as a 4-bit greyscale PNG, the panel's 16 greys.
fn grey_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let row = usize::try_from(width.div_ceil(2)).map_err(|e| e.to_string())?;
    let rows = usize::try_from(height).map_err(|e| e.to_string())?;
    let wide = usize::try_from(width).map_err(|e| e.to_string())?;
    let mut packed = vec![0u8; row.saturating_mul(rows)];
    let top = u16::from(GREY_LEVELS - 1);
    for (y, line) in rgba
        .chunks_exact(wide.saturating_mul(4))
        .take(rows)
        .enumerate()
    {
        for (x, p) in line.chunks_exact(4).enumerate() {
            // Rec. 601 luma, in integers.
            let luma =
                (u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114) / 1000;
            let level = u8::try_from((luma * u32::from(top) + 127) / 255).unwrap_or(15);
            let at = y.saturating_mul(row).saturating_add(x / 2);
            if let Some(byte) = packed.get_mut(at) {
                *byte |= if x % 2 == 0 { level << 4 } else { level };
            }
        }
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Four);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer
        .write_image_data(&packed)
        .map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

fn colour_jpeg(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    let picture = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| format!("a {width}x{height} page came back the wrong size"))?;
    let rgb = image::DynamicImage::ImageRgba8(picture).to_rgb8();
    let mut out = Vec::new();
    rgb.write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// One page as a picture: its bytes, its extension, and whether it is in
/// colour.
fn draw_page(
    book: &quire_pdf::PdfBook,
    index: usize,
    request: PagesRequest,
) -> Result<(Vec<u8>, &'static str, bool), String> {
    let size = book.page_size(index).map_err(|e| e.to_string())?;
    let (_, height) = fitted(size.width, size.height, request.bx);
    let raster = book.render(index, height).map_err(|e| e.to_string())?;
    let colour = !request.grey && has_colour(&raster.rgba);
    if colour {
        Ok((
            colour_jpeg(raster.width, raster.height, raster.rgba)?,
            "jpg",
            true,
        ))
    } else {
        Ok((
            grey_png(raster.width, raster.height, &raster.rgba)?,
            "png",
            false,
        ))
    }
}

/// A plain white page, standing in for one the engine could not draw so the
/// page numbers still match the book's.
fn blank_page(request: PagesRequest) -> Result<Vec<u8>, String> {
    let (width, height) = fitted(1.0, 1.414, request.bx);
    let white = vec![
        255u8;
        usize::try_from(width.saturating_mul(height).saturating_mul(4))
            .map_err(|e| e.to_string())?
    ];
    grey_png(width, height, &white)
}

/// Draws every page of the PDF at `src` into a CBZ at `tmp`.
pub fn draw(src: &Path, tmp: &Path, request: PagesRequest) -> Result<DrawStats, PagesError> {
    let read_err = |source| PagesError::Read {
        path: src.to_path_buf(),
        source,
    };
    let write_err = |source| PagesError::Write {
        path: tmp.to_path_buf(),
        source,
    };
    let zip_err = |e: zip::result::ZipError| write_err(std::io::Error::other(e));

    let bytes = std::fs::metadata(src).map_err(read_err)?.len();
    if bytes > MAX_SOURCE_BYTES {
        return Err(PagesError::TooLarge { bytes });
    }
    let book = quire_pdf::PdfBook::open(std::fs::read(src).map_err(read_err)?)?;
    let pages = book.page_count();
    if pages > MAX_PAGES {
        return Err(PagesError::TooManyPages { pages });
    }
    let digits = pages.to_string().len().max(4);

    let out = File::create(tmp).map_err(write_err)?;
    let mut zout = ZipWriter::new(BufWriter::new(out));
    // The pictures are compressed already, and stored members let a reader
    // take a page straight off the card.
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut stats = DrawStats::default();
    let mut first_failure = None;
    for index in 0..pages {
        stats.pages = stats.pages.saturating_add(1);
        let (picture, extension) = match draw_page(&book, index, request) {
            Ok((picture, extension, colour)) => {
                if colour {
                    stats.colour = stats.colour.saturating_add(1);
                } else {
                    stats.grey = stats.grey.saturating_add(1);
                }
                (picture, extension)
            }
            Err(reason) => {
                stats.failed = stats.failed.saturating_add(1);
                tracing::warn!(page = index.saturating_add(1), reason = %reason, "book pages: page drawn blank");
                first_failure.get_or_insert(reason);
                (
                    blank_page(request).map_err(|e| write_err(std::io::Error::other(e)))?,
                    "png",
                )
            }
        };
        let name = format!("page-{:0digits$}.{extension}", index.saturating_add(1));
        zout.start_file(name, stored).map_err(zip_err)?;
        zout.write_all(&picture).map_err(write_err)?;
    }
    if stats.failed == stats.pages {
        return Err(PagesError::NothingDrawn {
            pages,
            first: first_failure.unwrap_or_default(),
        });
    }
    let mut out = zout.finish().map_err(zip_err)?;
    out.flush().map_err(write_err)?;
    Ok(stats)
}

/// What one request knows when it decides, for the log line.
struct Decision<'a> {
    id: MediaId,
    src: &'a Path,
    request: PagesRequest,
    source_bytes: u64,
    served_bytes: u64,
    stats: DrawStats,
    started: Instant,
}

impl Decision<'_> {
    fn conclude(&self, outcome: PagesOutcome, reason: &str) {
        record_pages(outcome);
        macro_rules! line {
            ($level:ident, $msg:literal) => {
                tracing::$level!(
                    media.id = self.id,
                    path = %self.src.display(),
                    outcome = outcome.label(),
                    reason,
                    box_width = self.request.bx.width,
                    box_height = self.request.bx.height,
                    grey = self.request.grey,
                    source_bytes = self.source_bytes,
                    served_bytes = self.served_bytes,
                    pages = self.stats.pages,
                    colour_pages = self.stats.colour,
                    grey_pages = self.stats.grey,
                    failed_pages = self.stats.failed,
                    elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    $msg
                )
            };
        }
        match outcome {
            PagesOutcome::Drawn | PagesOutcome::CacheHit => line!(info, "book pages: decided"),
            _ => line!(warn, "book pages: not served"),
        }
    }
}

/// The page book to serve for a request to download the PDF at `src` (item
/// `id`) as pictures: the cached copy, drawing it first if need be.
///
/// # Errors
///
/// Whatever kept the copy from being made. Serving the PDF instead would hand
/// a client that asked for pictures a file it cannot open, so the caller
/// answers with the reason.
pub async fn get_or_draw(
    cache: Option<&ConvertedBookCache>,
    id: MediaId,
    edition: Option<&str>,
    src: &Path,
    request: PagesRequest,
) -> Result<PathBuf, PagesError> {
    let mut d = Decision {
        id,
        src,
        request,
        source_bytes: 0,
        served_bytes: 0,
        stats: DrawStats::default(),
        started: Instant::now(),
    };
    let found = find_or_draw(cache, id, edition, src, request, &mut d).await;
    match &found {
        Ok((_, outcome, reason)) => d.conclude(*outcome, reason),
        Err(e) => d.conclude(e.outcome(), &e.to_string()),
    }
    found.map(|(path, _, _)| path)
}

async fn find_or_draw(
    cache: Option<&ConvertedBookCache>,
    id: MediaId,
    edition: Option<&str>,
    src: &Path,
    request: PagesRequest,
    d: &mut Decision<'_>,
) -> Result<(PathBuf, PagesOutcome, &'static str), PagesError> {
    let meta = tokio::fs::metadata(src)
        .await
        .map_err(|source| PagesError::Read {
            path: src.to_path_buf(),
            source,
        })?;
    d.source_bytes = meta.len();
    let cache = cache.ok_or(PagesError::NoCache)?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |t| t.as_nanos());
    let copy = cache
        .pages_stem(id, edition, mtime_ns, d.source_bytes, &request.cache_tag())
        .with_extension("cbz");
    let hit = |d: &mut Decision<'_>, m: std::fs::Metadata| {
        d.served_bytes = m.len();
        (
            copy.clone(),
            PagesOutcome::CacheHit,
            "pictures of these pages for this box are cached",
        )
    };
    if let Ok(m) = tokio::fs::metadata(&copy).await {
        return Ok(hit(d, m));
    }

    let permit = tokio::time::timeout(DRAW_QUEUE_TIMEOUT, SLOTS.clone().acquire_owned())
        .await
        .map_err(|_| PagesError::Busy)?
        .map_err(|_| PagesError::Busy)?;
    // The request ahead in the queue may have drawn this very copy.
    if let Ok(m) = tokio::fs::metadata(&copy).await {
        return Ok(hit(d, m));
    }
    tokio::fs::create_dir_all(cache.pages_dir())
        .await
        .map_err(|source| PagesError::Write {
            path: cache.pages_dir(),
            source,
        })?;
    let tmp = tmp_path_for(&copy);
    let (src_owned, tmp_owned) = (src.to_path_buf(), tmp.clone());
    // Not cancelled when the request goes away: a client that gave up waiting
    // finds the finished copy when it asks again.
    let drawn = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        draw(&src_owned, &tmp_owned, request)
    })
    .await
    .map_err(|e| PagesError::Task(e.to_string()))
    .and_then(|r| r);
    let stats = match drawn {
        Ok(stats) => stats,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    d.stats = stats;
    let unchanged = tokio::fs::metadata(src)
        .await
        .is_ok_and(|m| m.len() == d.source_bytes && m.modified().ok() == meta.modified().ok());
    if !unchanged {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(PagesError::Changed);
    }
    if let Err(source) = tokio::fs::rename(&tmp, &copy).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(PagesError::Write { path: copy, source });
    }
    d.served_bytes = tokio::fs::metadata(&copy).await.map_or(0, |m| m.len());
    Ok((
        copy,
        PagesOutcome::Drawn,
        "the pages were drawn to fit the box",
    ))
}

/// A scratch path beside `copy` that no other drawing uses.
fn tmp_path_for(copy: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    copy.with_extension(format!("cbz.tmp{}-{n}", std::process::id()))
}

#[cfg(test)]
#[path = "book_pages_tests.rs"]
mod tests;
