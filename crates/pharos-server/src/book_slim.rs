//! Slimmed EPUB copies for e-readers with a small download and image budget
//! (B1451, ADR-0138).
//!
//! The Kobo refuses a download over 32 MB and keeps only 8 MB of pictures per
//! book, 2 MB each. A big EPUB is big because of its pictures, so the server
//! hands such a client a copy whose pictures are scaled to the screen. The
//! original file is never touched: the copy is built into the book cache
//! directory, keyed by the source file's mtime and size, so an edited book
//! gets a new copy and a repeat request costs a `stat`.
//!
//! Every decision lands on `pharos_book_slim_total{outcome}` and in one
//! `book slim:` log line carrying the inputs and the reason.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, UNIX_EPOCH};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use pharos_core::MediaId;
use tokio::sync::Semaphore;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::api::jellyfin::ci_query;
use crate::book_convert::ConvertedBookCache;

/// Largest bounding edge a request may ask for; anything above reads as absent.
const MAX_BOX_EDGE: u32 = 16_384;

/// Most entries a book may have and still be re-packed.
const MAX_ENTRIES: usize = 20_000;

/// Largest single picture (uncompressed) that is decoded.
const MAX_IMAGE_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// Most uncompressed picture bytes decoded for one book; later pictures are
/// copied as they are.
const MAX_BOOK_IMAGE_BYTES: u64 = 1024 * 1024 * 1024;

/// Largest width or height a decode may claim, before any pixel is allocated.
const MAX_DIMENSION: u32 = 12_000;

/// Most memory one decode may allocate.
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

const JPEG_QUALITY: u8 = 80;

/// Books are re-packed on blocking threads, at most this many at once.
const SLIM_SLOTS: usize = 2;

/// How long a request waits for a slot before the original is served.
const SLIM_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);

static SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(SLIM_SLOTS)));

/// The box pictures are scaled to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlimBox {
    pub width: u32,
    pub height: u32,
}

impl SlimBox {
    /// The box a request asks for with `maxWidth` / `maxHeight` (matched
    /// case-insensitively, the tightest repeat wins), or `None` when it asks
    /// for neither. An unparseable, zero or absurd edge reads as absent; one
    /// edge alone leaves the other unbounded.
    pub fn from_query(qs: &str) -> Option<Self> {
        let edge = |key: &str| {
            ci_query::values(qs, key)
                .filter_map(|v| v.parse::<u32>().ok())
                .filter(|n| (1..=MAX_BOX_EDGE).contains(n))
                .min()
        };
        let (width, height) = (edge("maxWidth"), edge("maxHeight"));
        if width.is_none() && height.is_none() {
            return None;
        }
        Some(Self {
            width: width.unwrap_or(MAX_BOX_EDGE),
            height: height.unwrap_or(MAX_BOX_EDGE),
        })
    }
}

/// What a request for a slimmed copy ended in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlimOutcome {
    /// A slimmed copy was built and cached, and is what is served.
    Slimmed,
    /// A previously built slimmed copy is served from the cache.
    CacheHit,
    /// An earlier decision that the original is the answer is served from the
    /// cache, without opening the book.
    CacheHitOriginal,
    /// No picture is larger than the box; the original is served.
    AlreadyFits,
    /// The re-packed copy was not smaller; the original is served.
    NotSmaller,
    /// No book cache directory is configured, so nothing can be stored; the
    /// original is served.
    NoCache,
    /// The book could not be read or re-packed; the original is served.
    Failed,
}

impl SlimOutcome {
    pub const ALL: [SlimOutcome; 7] = [
        SlimOutcome::Slimmed,
        SlimOutcome::CacheHit,
        SlimOutcome::CacheHitOriginal,
        SlimOutcome::AlreadyFits,
        SlimOutcome::NotSmaller,
        SlimOutcome::NoCache,
        SlimOutcome::Failed,
    ];

    /// The `outcome` label value. A dashboard contract: do not rename.
    pub fn label(self) -> &'static str {
        match self {
            SlimOutcome::Slimmed => "slimmed",
            SlimOutcome::CacheHit => "cache_hit",
            SlimOutcome::CacheHitOriginal => "cache_hit_original",
            SlimOutcome::AlreadyFits => "already_fits",
            SlimOutcome::NotSmaller => "not_smaller",
            SlimOutcome::NoCache => "no_cache",
            SlimOutcome::Failed => "failed",
        }
    }
}

/// Count one slim decision.
pub fn record_slim(outcome: SlimOutcome) {
    metrics::counter!("pharos_book_slim_total", "outcome" => outcome.label()).increment(1);
}

/// Why a book could not be re-packed.
#[derive(Debug, thiserror::Error)]
pub enum SlimError {
    #[error("open {path}: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("read the zip {path}: {source}")]
    Zip {
        path: PathBuf,
        source: zip::result::ZipError,
    },
    #[error("write the slimmed copy {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the book has no `mimetype` entry reading application/epub+zip")]
    NoMimetype,
    #[error("the book has {entries} entries; at most {MAX_ENTRIES} are re-packed")]
    TooManyEntries { entries: usize },
    #[error("re-pack task: {0}")]
    Task(String),
    #[error("all {SLIM_SLOTS} re-pack slots stayed busy for {SLIM_QUEUE_TIMEOUT:?}")]
    Busy,
}

/// What one re-pack did, for the log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RepackStats {
    pub pictures: u32,
    pub scaled: u32,
    pub fit: u32,
    pub failed: u32,
    pub skipped_over_budget: u32,
}

fn picture_format(name: &str) -> Option<ImageFormat> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "png" => Some(ImageFormat::Png),
        "webp" => Some(ImageFormat::WebP),
        "gif" => Some(ImageFormat::Gif),
        _ => None,
    }
}

/// `bytes` scaled to fit `bx` and re-encoded in its own format, or `None`
/// when the picture already fits (or is an animation, which a single frame
/// would destroy). The camera's orientation is applied, since the re-encode
/// drops the EXIF that carried it.
fn scale_picture(
    bytes: &[u8],
    format: ImageFormat,
    bx: SlimBox,
) -> Result<Option<Vec<u8>>, String> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits.clone());
    let (w, h) = reader.into_dimensions().map_err(|e| e.to_string())?;
    if w > MAX_DIMENSION || h > MAX_DIMENSION {
        return Err(format!("{w}x{h} exceeds the {MAX_DIMENSION} decode edge"));
    }
    if w <= bx.width && h <= bx.height {
        return Ok(None);
    }
    if format == ImageFormat::Gif {
        let frames = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
            .map_err(|e| e.to_string())?
            .into_frames()
            .take(2)
            .count();
        if frames > 1 {
            return Ok(None);
        }
    }

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut picture = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    picture.apply_orientation(orientation);
    if picture.width() <= bx.width && picture.height() <= bx.height {
        return Ok(None);
    }
    let picture = picture.resize(bx.width, bx.height, FilterType::Lanczos3);

    let mut out = Vec::new();
    if format == ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(picture.to_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))
            .map_err(|e| e.to_string())?;
    } else {
        let picture = if picture.color().has_alpha() {
            DynamicImage::ImageRgba8(picture.to_rgba8())
        } else {
            DynamicImage::ImageRgb8(picture.to_rgb8())
        };
        picture
            .write_to(&mut Cursor::new(&mut out), format)
            .map_err(|e| e.to_string())?;
    }
    Ok(Some(out))
}

/// Re-pack the EPUB at `src` into `tmp` with every picture larger than `bx`
/// scaled to fit. `mimetype` comes first and stored (OCF); every other entry
/// is copied compressed-byte for compressed-byte, except a scaled picture,
/// which keeps its name and format and so its manifest media type.
pub fn repack(src: &Path, tmp: &Path, bx: SlimBox) -> Result<RepackStats, SlimError> {
    let zip_err = |source| SlimError::Zip {
        path: src.to_path_buf(),
        source,
    };
    let read_err = |source| SlimError::Open {
        path: src.to_path_buf(),
        source,
    };
    let write_err = |source| SlimError::Write {
        path: tmp.to_path_buf(),
        source,
    };
    let zip_write_err = |e: zip::result::ZipError| write_err(std::io::Error::other(e));

    let file = File::open(src).map_err(read_err)?;
    let mut zin = ZipArchive::new(BufReader::new(file)).map_err(zip_err)?;
    if zin.len() > MAX_ENTRIES {
        return Err(SlimError::TooManyEntries { entries: zin.len() });
    }
    let mimetype_at = zin
        .index_for_name("mimetype")
        .ok_or(SlimError::NoMimetype)?;
    let mut mimetype = Vec::new();
    zin.by_index(mimetype_at)
        .map_err(zip_err)?
        .take(64)
        .read_to_end(&mut mimetype)
        .map_err(read_err)?;
    if mimetype != b"application/epub+zip" {
        return Err(SlimError::NoMimetype);
    }

    let out = File::create(tmp).map_err(write_err)?;
    let mut zout = ZipWriter::new(out);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zout.start_file("mimetype", stored).map_err(zip_write_err)?;
    zout.write_all(&mimetype).map_err(write_err)?;

    let mut stats = RepackStats::default();
    let mut budget = MAX_BOOK_IMAGE_BYTES;
    for i in 0..zin.len() {
        if i == mimetype_at {
            continue;
        }
        let (name, size) = {
            let raw = zin.by_index_raw(i).map_err(zip_err)?;
            (
                raw.name().map(Cow::into_owned).unwrap_or_default(),
                raw.size(),
            )
        };
        let Some(format) = picture_format(&name) else {
            zout.raw_copy_file(zin.by_index_raw(i).map_err(zip_err)?)
                .map_err(zip_write_err)?;
            continue;
        };
        stats.pictures = stats.pictures.saturating_add(1);
        if size > MAX_IMAGE_ENTRY_BYTES || size > budget {
            stats.skipped_over_budget = stats.skipped_over_budget.saturating_add(1);
            zout.raw_copy_file(zin.by_index_raw(i).map_err(zip_err)?)
                .map_err(zip_write_err)?;
            continue;
        }
        budget = budget.saturating_sub(size);
        let mut bytes = Vec::new();
        let read = zin
            .by_index(i)
            .map_err(zip_err)?
            .take(size.saturating_add(1))
            .read_to_end(&mut bytes);
        let scaled = match read {
            Ok(n) if u64::try_from(n).is_ok_and(|n| n <= size) => scale_picture(&bytes, format, bx),
            Ok(_) => Err("the entry is larger than its header says".to_owned()),
            Err(e) => Err(e.to_string()),
        };
        match scaled {
            Ok(Some(new)) if new.len() < bytes.len() => {
                stats.scaled = stats.scaled.saturating_add(1);
                zout.start_file(&name, stored).map_err(zip_write_err)?;
                zout.write_all(&new).map_err(write_err)?;
                continue;
            }
            Ok(None) => stats.fit = stats.fit.saturating_add(1),
            Ok(Some(_)) => {}
            Err(reason) => {
                stats.failed = stats.failed.saturating_add(1);
                tracing::warn!(
                    entry = %name,
                    bytes = size,
                    reason = %reason,
                    "book slim: picture kept as it is"
                );
            }
        }
        zout.raw_copy_file(zin.by_index_raw(i).map_err(zip_err)?)
            .map_err(zip_write_err)?;
    }
    zout.finish().map_err(zip_write_err)?;
    Ok(stats)
}

/// What one request knows when it decides, for the log line.
struct Decision<'a> {
    id: MediaId,
    src: &'a Path,
    bx: SlimBox,
    source_bytes: u64,
    served_bytes: u64,
    stats: RepackStats,
}

impl Decision<'_> {
    /// Count the outcome and log it with the inputs and the reason.
    fn conclude(&self, outcome: SlimOutcome, reason: &str) {
        record_slim(outcome);
        let Self {
            id,
            src,
            bx,
            source_bytes,
            served_bytes,
            stats,
        } = self;
        macro_rules! line {
            ($level:ident, $msg:literal) => {
                tracing::$level!(
                    media.id = *id,
                    path = %src.display(),
                    outcome = outcome.label(),
                    reason,
                    box_width = bx.width,
                    box_height = bx.height,
                    source_bytes = *source_bytes,
                    served_bytes = *served_bytes,
                    pictures = stats.pictures,
                    scaled = stats.scaled,
                    fit = stats.fit,
                    failed = stats.failed,
                    skipped_over_budget = stats.skipped_over_budget,
                    $msg
                )
            };
        }
        if outcome == SlimOutcome::Failed {
            line!(warn, "book slim: failed; serving the original");
        } else {
            line!(info, "book slim: decided");
        }
    }
}

/// The file to serve for a request to download `src` (item `id`) scaled to
/// `bx`: the cached slimmed copy, building it first if need be, or `src`
/// itself when a slimmed copy is not the answer. Never fails: the original is
/// the fallback, as for a failed Kindle conversion, and the cause is on the
/// counter and in the log.
pub async fn get_or_slim(
    cache: Option<&ConvertedBookCache>,
    id: MediaId,
    edition: Option<&str>,
    src: &Path,
    bx: SlimBox,
) -> PathBuf {
    let original = src.to_path_buf();
    let mut d = Decision {
        id,
        src,
        bx,
        source_bytes: 0,
        served_bytes: 0,
        stats: RepackStats::default(),
    };
    let meta = match tokio::fs::metadata(src).await {
        Ok(m) => m,
        Err(e) => {
            d.conclude(SlimOutcome::Failed, &format!("stat the source: {e}"));
            return original;
        }
    };
    d.source_bytes = meta.len();
    d.served_bytes = meta.len();
    let Some(cache) = cache else {
        d.conclude(
            SlimOutcome::NoCache,
            "no book cache directory is configured",
        );
        return original;
    };
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |t| t.as_nanos());
    let stem = cache.slim_stem(id, edition, mtime_ns, d.source_bytes, bx);
    let slim = stem.with_extension("epub");
    let marker = stem.with_extension("orig");

    if let Ok(m) = tokio::fs::metadata(&slim).await {
        d.served_bytes = m.len();
        d.conclude(
            SlimOutcome::CacheHit,
            "a slimmed copy for this file and box is cached",
        );
        return slim;
    }
    if tokio::fs::try_exists(&marker).await.unwrap_or(false) {
        d.conclude(
            SlimOutcome::CacheHitOriginal,
            "an earlier pass found the original is the answer for this file and box",
        );
        return original;
    }

    let permit = match tokio::time::timeout(SLIM_QUEUE_TIMEOUT, SLOTS.clone().acquire_owned()).await
    {
        Ok(Ok(p)) => p,
        _ => {
            d.conclude(SlimOutcome::Failed, &SlimError::Busy.to_string());
            return original;
        }
    };
    if let Err(e) = tokio::fs::create_dir_all(cache.slim_dir()).await {
        d.conclude(
            SlimOutcome::Failed,
            &format!("create {}: {e}", cache.slim_dir().display()),
        );
        return original;
    }
    let tmp = slim.with_extension(format!("epub.tmp{}", std::process::id()));
    let (src_owned, tmp_owned) = (original.clone(), tmp.clone());
    let packed = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        repack(&src_owned, &tmp_owned, bx)
    })
    .await
    .map_err(|e| SlimError::Task(e.to_string()))
    .and_then(|r| r);

    d.stats = match packed {
        Ok(s) => s,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            d.conclude(SlimOutcome::Failed, &e.to_string());
            return original;
        }
    };
    let slimmed_bytes = tokio::fs::metadata(&tmp)
        .await
        .map_or(u64::MAX, |m| m.len());
    let verdict = if d.stats.scaled == 0 {
        Some((
            SlimOutcome::AlreadyFits,
            "no picture is larger than the box",
        ))
    } else if slimmed_bytes >= d.source_bytes {
        Some((SlimOutcome::NotSmaller, "the re-packed copy is not smaller"))
    } else {
        None
    };
    if let Some((outcome, reason)) = verdict {
        let _ = tokio::fs::remove_file(&tmp).await;
        if let Err(e) = tokio::fs::write(&marker, b"").await {
            tracing::warn!(error = %e, marker = %marker.display(), "book slim: could not store the decision");
        }
        d.conclude(outcome, reason);
        return original;
    }
    if let Err(e) = tokio::fs::rename(&tmp, &slim).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        d.conclude(
            SlimOutcome::Failed,
            &format!("store {}: {e}", slim.display()),
        );
        return original;
    }
    d.served_bytes = slimmed_bytes;
    d.conclude(
        SlimOutcome::Slimmed,
        "pictures larger than the box were scaled to fit",
    );
    slim
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use std::collections::HashSet;

    #[test]
    fn the_box_reads_case_insensitively_and_junk_is_no_box() {
        let bx = |qs| SlimBox::from_query(qs);
        assert_eq!(
            bx("api_key=x&MaxWidth=1264&maxheight=1680"),
            Some(SlimBox {
                width: 1264,
                height: 1680
            })
        );
        assert_eq!(
            bx("maxWidth=900&maxWidth=700"),
            Some(SlimBox {
                width: 700,
                height: MAX_BOX_EDGE
            })
        );
        assert_eq!(bx(""), None);
        assert_eq!(bx("maxWidth=abc&maxHeight=0&maxWidth=99999"), None);
    }

    fn encode(format: ImageFormat, w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x * y) % 239) as u8])
        }));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), format).unwrap();
        out
    }

    #[test]
    fn every_format_scales_into_the_box_and_keeps_its_format() {
        let bx = SlimBox {
            width: 50,
            height: 50,
        };
        for format in [
            ImageFormat::Jpeg,
            ImageFormat::Png,
            ImageFormat::WebP,
            ImageFormat::Gif,
        ] {
            let big = encode(format, 200, 120);
            let scaled = scale_picture(&big, format, bx).unwrap().unwrap();
            assert_eq!(image::guess_format(&scaled).unwrap(), format);
            let img = image::load_from_memory(&scaled).unwrap();
            assert_eq!((img.width(), img.height()), (50, 30), "{format:?}");
            let small = encode(format, 40, 40);
            assert_eq!(scale_picture(&small, format, bx).unwrap(), None);
        }
    }

    #[test]
    fn a_picture_that_does_not_decode_is_an_error_not_a_panic() {
        let junk = vec![0xff; 64];
        assert!(scale_picture(
            &junk,
            ImageFormat::Jpeg,
            SlimBox {
                width: 1,
                height: 1
            }
        )
        .is_err());
    }

    #[test]
    fn every_outcome_has_its_own_label() {
        let labels: HashSet<_> = SlimOutcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), SlimOutcome::ALL.len());
    }

    /// Disarm check: make `record_slim` a no-op and the counter is absent.
    #[test]
    fn a_decision_is_counted_under_its_label() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            record_slim(SlimOutcome::Slimmed);
            record_slim(SlimOutcome::Slimmed);
            record_slim(SlimOutcome::CacheHit);
        });
        let count = |outcome: &str| -> u64 {
            snapshotter
                .snapshot()
                .into_vec()
                .into_iter()
                .filter(|(k, ..)| {
                    k.key().name() == "pharos_book_slim_total"
                        && k.key()
                            .labels()
                            .any(|l| l.key() == "outcome" && l.value() == outcome)
                })
                .map(|(.., v)| match v {
                    DebugValue::Counter(n) => n,
                    _ => 0,
                })
                .sum()
        };
        assert_eq!(count("slimmed"), 2);
        assert_eq!(count("cache_hit"), 1);
        assert_eq!(count("failed"), 0);
    }
}
