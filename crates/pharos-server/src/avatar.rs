//! A user's avatar: an uploaded picture checked, cropped square and
//! re-encoded before it is stored, so what the server keeps and serves is
//! always an image it wrote itself (V362). Stored as the user's
//! [`AVATAR_PREF_KEY`] pref, with its tag beside it under
//! [`AVATAR_TAG_PREF_KEY`] so a user listing need not read every picture.
//! The native `Prefs` API offers neither key, so only these functions write
//! them.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use pharos_core::{DomainError, UserId, UserPrefStore};
use tokio::sync::Semaphore;

use crate::state::Stores;

/// The user pref an avatar is stored under.
pub const AVATAR_PREF_KEY: &str = "avatar";

/// The user pref an avatar's [`tag_of`] is stored under, written after the
/// picture so a tag never names bytes that were not stored.
pub const AVATAR_TAG_PREF_KEY: &str = "avatar_tag";

/// Largest upload accepted, before any decode. A phone photo is well under
/// this; anything bigger is refused unread.
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

/// Width and height of a stored avatar, in pixels. Big enough for a
/// high-density profile picture, small enough to stay tens of kilobytes.
pub const SIDE: u32 = 512;

/// Largest width or height a decode may claim; a header claiming more is
/// refused before pixels are allocated (a decompression bomb).
const MAX_DIMENSION: u32 = 8192;

/// Most memory one decode may allocate.
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

const JPEG_QUALITY: u8 = 85;

/// Decodes run on blocking threads, at most this many at once, so a burst
/// of uploads cannot hold more than this many decode buffers.
const DECODE_SLOTS: usize = 2;

/// How long an upload waits for a decode slot before it is refused busy.
const DECODE_QUEUE_TIMEOUT: Duration = Duration::from_secs(10);

static SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(DECODE_SLOTS)));

/// Why an upload was not stored.
#[derive(Debug, thiserror::Error)]
pub enum AvatarError {
    #[error("the picture is empty")]
    Empty,
    #[error("the picture is {bytes} bytes; at most {MAX_UPLOAD_BYTES} are accepted")]
    TooLarge { bytes: usize },
    /// The bytes are not PNG, JPEG, WebP or GIF, whatever the upload said.
    #[error("not a PNG, JPEG, WebP or GIF picture")]
    Unsupported,
    /// The bytes start like a picture but do not decode within the limits.
    #[error("the picture could not be read: {0}")]
    Decode(String),
    #[error("could not encode the avatar: {0}")]
    Encode(String),
    #[error("too many pictures are being processed; try again")]
    Busy,
    #[error("store: {0}")]
    Store(#[from] DomainError),
}

/// A stored avatar: its bytes, the content type to serve them with, and a
/// tag that changes whenever they do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avatar {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub tag: String,
}

impl Avatar {
    fn from_stored(bytes: Vec<u8>) -> Self {
        // Only `normalise` writes this pref, so the bytes are its PNG or JPEG.
        let content_type = match image::guess_format(&bytes) {
            Ok(ImageFormat::Png) => "image/png",
            _ => "image/jpeg",
        };
        let tag = tag_of(&bytes);
        Self {
            bytes,
            content_type,
            tag,
        }
    }
}

/// A short tag for `bytes`: the hex of their xxh3 hash.
pub fn tag_of(bytes: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(bytes))
}

/// Checks and re-encodes an upload: the format is read from the bytes (any
/// declared type is ignored), the decode is bounded by [`MAX_DIMENSION`] and
/// [`MAX_DECODE_ALLOC`], the camera's orientation is applied, the picture is
/// cropped to a centred square of at most [`SIDE`] pixels, and written
/// fresh as JPEG, or PNG when it has transparency. Nothing of the upload
/// but its pixels survives, so metadata (EXIF location) is dropped.
pub fn normalise(upload: &[u8]) -> Result<Vec<u8>, AvatarError> {
    if upload.is_empty() {
        return Err(AvatarError::Empty);
    }
    if upload.len() > MAX_UPLOAD_BYTES {
        return Err(AvatarError::TooLarge {
            bytes: upload.len(),
        });
    }
    let format = match image::guess_format(upload) {
        Ok(f @ (ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif)) => f,
        _ => return Err(AvatarError::Unsupported),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(upload), format);
    reader.limits(limits);
    let decode = |e: image::ImageError| AvatarError::Decode(e.to_string());
    let mut decoder = reader.into_decoder().map_err(decode)?;
    let orientation = decoder.orientation().map_err(decode)?;
    let mut picture = DynamicImage::from_decoder(decoder).map_err(decode)?;
    picture.apply_orientation(orientation);

    let side = picture.width().min(picture.height()).clamp(1, SIDE);
    let square = picture.resize_to_fill(side, side, FilterType::Lanczos3);
    let mut out = Vec::new();
    let encode = |e: image::ImageError| AvatarError::Encode(e.to_string());
    // An alpha channel alone is not transparency: a GIF always decodes to
    // RGBA, so only a pixel that is not fully opaque keeps the PNG.
    let rgba = square.color().has_alpha().then(|| square.to_rgba8());
    if let Some(rgba) = rgba.filter(|p| p.pixels().any(|px| px[3] < u8::MAX)) {
        DynamicImage::ImageRgba8(rgba)
            .write_with_encoder(PngEncoder::new(&mut out))
            .map_err(encode)?;
    } else {
        DynamicImage::ImageRgb8(square.to_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))
            .map_err(encode)?;
    }
    Ok(out)
}

/// [`normalise`] on a blocking thread, at most [`DECODE_SLOTS`] at once.
pub async fn normalise_off_worker(upload: Vec<u8>) -> Result<Vec<u8>, AvatarError> {
    let permit = tokio::time::timeout(DECODE_QUEUE_TIMEOUT, SLOTS.clone().acquire_owned())
        .await
        .map_err(|_| AvatarError::Busy)?
        .map_err(|_| AvatarError::Busy)?;
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        normalise(&upload)
    })
    .await
    .map_err(|e| AvatarError::Decode(format!("decode task: {e}")))?;
    result
}

/// `user`'s avatar, or `None` when they have not set one.
pub async fn load(stores: &Stores, user: UserId) -> Result<Option<Avatar>, DomainError> {
    Ok(stores
        .get_user_pref(user, AVATAR_PREF_KEY)
        .await?
        .filter(|bytes| !bytes.is_empty())
        .map(Avatar::from_stored))
}

/// The tag of `user`'s avatar, for `UserDto.PrimaryImageTag`. A store
/// failure is logged and read as no avatar, so the reply still answers.
pub async fn tag(stores: &Stores, user: UserId) -> Option<String> {
    match stores.get_user_pref(user, AVATAR_TAG_PREF_KEY).await {
        Ok(stored) => stored.and_then(|t| String::from_utf8(t).ok()),
        Err(e) => {
            tracing::warn!(user.id = %user.0.simple(), error = %e, "avatar: tag lookup failed");
            None
        }
    }
}

/// Every user's avatar tag in one read, for a user listing. A store failure
/// is logged and read as no avatars.
pub async fn tags(stores: &Stores) -> HashMap<UserId, String> {
    match stores.user_prefs_for_key(AVATAR_TAG_PREF_KEY).await {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|(user, t)| Some((user, String::from_utf8(t).ok()?)))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "avatar: tag listing failed");
            HashMap::new()
        }
    }
}

/// Checks, re-encodes and stores `upload` as `user`'s avatar; returns the
/// new tag. Logged with the sizes either way, never the bytes.
pub async fn set(stores: &Stores, user: UserId, upload: Vec<u8>) -> Result<String, AvatarError> {
    let uploaded = upload.len();
    let result = async {
        let bytes = normalise_off_worker(upload).await?;
        stores.set_user_pref(user, AVATAR_PREF_KEY, &bytes).await?;
        let tag = tag_of(&bytes);
        stores
            .set_user_pref(user, AVATAR_TAG_PREF_KEY, tag.as_bytes())
            .await?;
        Ok::<_, AvatarError>((tag, bytes.len()))
    }
    .await;
    match &result {
        Ok((tag, stored)) => {
            metrics::counter!("pharos_avatar_uploads_total", "outcome" => "ok").increment(1);
            tracing::info!(user.id = %user.0.simple(), uploaded, stored, tag, "avatar set");
        }
        Err(e) => {
            metrics::counter!("pharos_avatar_uploads_total", "outcome" => outcome_label(e))
                .increment(1);
            tracing::warn!(user.id = %user.0.simple(), uploaded, error = %e, "avatar refused");
        }
    }
    result.map(|(tag, _)| tag)
}

/// Removes `user`'s avatar; `true` when there was one.
pub async fn remove(stores: &Stores, user: UserId) -> Result<bool, DomainError> {
    // The tag first, so a listing never names a picture that is gone.
    stores.delete_user_pref(user, AVATAR_TAG_PREF_KEY).await?;
    let removed = stores.delete_user_pref(user, AVATAR_PREF_KEY).await?;
    tracing::info!(user.id = %user.0.simple(), removed, "avatar removed");
    Ok(removed)
}

fn outcome_label(e: &AvatarError) -> &'static str {
    match e {
        AvatarError::Empty => "empty",
        AvatarError::TooLarge { .. } => "too_large",
        AvatarError::Unsupported => "unsupported",
        AvatarError::Decode(_) => "decode",
        AvatarError::Encode(_) => "encode",
        AvatarError::Busy => "busy",
        AvatarError::Store(_) => "store",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    fn encoded(picture: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        picture.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn decoded(bytes: &[u8]) -> DynamicImage {
        image::load_from_memory(bytes).unwrap()
    }

    #[test]
    fn a_wide_photo_becomes_a_centred_square_jpeg_of_at_most_side() {
        let photo = RgbImage::from_fn(1600, 900, |x, _| {
            if x < 800 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        });
        let out = normalise(&encoded(photo.into(), ImageFormat::Png)).unwrap();
        assert_eq!(image::guess_format(&out).unwrap(), ImageFormat::Jpeg);
        let back = decoded(&out);
        assert_eq!((back.width(), back.height()), (SIDE, SIDE));
    }

    #[test]
    fn a_small_picture_is_not_scaled_up() {
        let small = RgbImage::from_pixel(40, 64, Rgb([10, 20, 30]));
        let back = decoded(&normalise(&encoded(small.into(), ImageFormat::Jpeg)).unwrap());
        assert_eq!((back.width(), back.height()), (40, 40));
    }

    #[test]
    fn transparency_is_kept_as_png() {
        let logo = RgbaImage::from_pixel(100, 100, Rgba([0, 0, 0, 0]));
        let out = normalise(&encoded(logo.into(), ImageFormat::Png)).unwrap();
        assert_eq!(image::guess_format(&out).unwrap(), ImageFormat::Png);
    }

    #[test]
    fn an_opaque_picture_with_an_alpha_channel_becomes_jpeg() {
        let photo = RgbImage::from_pixel(30, 30, Rgb([200, 100, 50]));
        // GIF decodes to RGBA whatever it holds; a PNG may carry an unused
        // alpha channel.
        let opaque_rgba = RgbaImage::from_pixel(30, 30, Rgba([200, 100, 50, 255]));
        for upload in [
            encoded(photo.into(), ImageFormat::Gif),
            encoded(opaque_rgba.into(), ImageFormat::Png),
        ] {
            let out = normalise(&upload).unwrap();
            assert_eq!(image::guess_format(&out).unwrap(), ImageFormat::Jpeg);
        }
    }

    #[test]
    fn webp_and_gif_are_accepted() {
        let pic = RgbImage::from_pixel(20, 20, Rgb([1, 2, 3]));
        for format in [ImageFormat::WebP, ImageFormat::Gif] {
            let out = normalise(&encoded(pic.clone().into(), format));
            assert!(out.is_ok(), "{format:?}: {out:?}");
        }
    }

    #[test]
    fn what_is_not_a_picture_is_refused_whatever_it_claims() {
        assert!(matches!(normalise(b""), Err(AvatarError::Empty)));
        assert!(matches!(
            normalise(b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
            Err(AvatarError::Unsupported)
        ));
        assert!(matches!(
            normalise(b"<html><script>alert(1)</script></html>"),
            Err(AvatarError::Unsupported)
        ));
        // A PNG signature followed by garbage starts like a picture but is not one.
        let mut fake = b"\x89PNG\r\n\x1a\n".to_vec();
        fake.extend_from_slice(&[0u8; 64]);
        assert!(matches!(normalise(&fake), Err(AvatarError::Decode(_))));
    }

    #[test]
    fn an_upload_over_the_cap_is_refused_unread() {
        let big = vec![0u8; MAX_UPLOAD_BYTES + 1];
        assert!(matches!(normalise(&big), Err(AvatarError::TooLarge { .. })));
    }

    /// A tiny file whose header claims a picture past [`MAX_DIMENSION`] is
    /// refused before its pixels are allocated.
    #[test]
    fn a_header_claiming_huge_dimensions_is_refused() {
        let small = RgbImage::from_pixel(1, 1, Rgb([0, 0, 0]));
        let mut png = encoded(small.into(), ImageFormat::Png);
        // IHDR's type, width and height sit at bytes 12..24 and its CRC at
        // 29..33; claim 60000 x 60000 with a CRC that matches, so the size
        // limit (not a checksum) is what refuses it.
        png.splice(
            16..24,
            [60_000u32.to_be_bytes(), 60_000u32.to_be_bytes()].concat(),
        );
        let crc = crc32(png.get(12..29).unwrap());
        png.splice(29..33, crc.to_be_bytes());
        match normalise(&png) {
            Err(AvatarError::Decode(e)) => assert!(e.to_lowercase().contains("limit"), "{e}"),
            other => panic!("expected a limit refusal, got {other:?}"),
        }
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn the_stored_bytes_keep_none_of_the_upload() {
        let pic = RgbImage::from_pixel(64, 64, Rgb([9, 9, 9]));
        let mut upload = encoded(pic.into(), ImageFormat::Jpeg);
        // Bytes after the JPEG end marker survive a naive copy.
        upload.extend_from_slice(b"SECRET-TRAILER");
        let out = normalise(&upload).unwrap();
        assert!(!out.windows(14).any(|w| w == b"SECRET-TRAILER"));
    }

    #[test]
    fn the_tag_follows_the_bytes() {
        assert_eq!(tag_of(b"a"), tag_of(b"a"));
        assert_ne!(tag_of(b"a"), tag_of(b"b"));
        assert_eq!(tag_of(b"a").len(), 16);
    }
}
