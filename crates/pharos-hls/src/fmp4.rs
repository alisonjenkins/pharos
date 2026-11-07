//! Read-only, bounds-checked ISO BMFF (fMP4) box inspection for init and media segments.

use std::fmt;

/// Structural or bounds failure when inspecting an fMP4 segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fmp4Error {
    Truncated { at: usize },
    BadBoxSize { at: usize, size: u64 },
    Missing(&'static str),
    UnsupportedVersion { name: &'static str, version: u8 },
}

impl fmt::Display for Fmp4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { at } => write!(f, "fMP4 box truncated at byte offset {at}"),
            Self::BadBoxSize { at, size } => {
                write!(f, "invalid fMP4 box size {size} at byte offset {at}")
            }
            Self::Missing(name) => write!(f, "missing required fMP4 box: {name}"),
            Self::UnsupportedVersion { name, version } => {
                write!(f, "unsupported {name} box version {version}")
            }
        }
    }
}

impl std::error::Error for Fmp4Error {}

struct ParsedBox<'a> {
    at: usize,
    fourcc: [u8; 4],
    payload: &'a [u8],
    payload_offset: usize,
}

struct BoxIter<'a> {
    buf: &'a [u8],
    base_offset: usize,
    pos: usize,
}

impl<'a> BoxIter<'a> {
    fn new(buf: &'a [u8], base_offset: usize) -> Self {
        Self {
            buf,
            base_offset,
            pos: 0,
        }
    }

    fn next_box(&mut self) -> Result<Option<ParsedBox<'a>>, Fmp4Error> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let at = self.base_offset.saturating_add(self.pos);
        let rem = self
            .buf
            .get(self.pos..)
            .ok_or(Fmp4Error::Truncated { at })?;

        let size32 = read_u32_be(rem, 0, at)?;
        let fourcc = read_fourcc(rem, 4, at)?;

        let (header_len, total_len) = match size32 {
            0 => (8usize, rem.len()),
            1 => {
                let size64 = read_u64_be(rem, 8, at)?;
                if size64 < 16 {
                    return Err(Fmp4Error::BadBoxSize { at, size: size64 });
                }
                if size64 > rem.len() as u64 {
                    return Err(Fmp4Error::Truncated { at });
                }
                let len = usize::try_from(size64).map_err(|_| Fmp4Error::Truncated { at })?;
                (16usize, len)
            }
            2..=7 => {
                return Err(Fmp4Error::BadBoxSize {
                    at,
                    size: u64::from(size32),
                });
            }
            n => {
                let size64 = u64::from(n);
                if size64 > rem.len() as u64 {
                    return Err(Fmp4Error::Truncated { at });
                }
                let len = usize::try_from(size64).map_err(|_| Fmp4Error::Truncated { at })?;
                (8usize, len)
            }
        };

        let payload = rem
            .get(header_len..total_len)
            .ok_or(Fmp4Error::Truncated { at })?;
        let payload_offset = at.saturating_add(header_len);
        self.pos = self.pos.saturating_add(total_len);

        Ok(Some(ParsedBox {
            at,
            fourcc,
            payload,
            payload_offset,
        }))
    }
}

/// Each track's `(track_id, timescale)` from an init segment's `moov/trak/(tkhd, mdia/mdhd)`.
pub fn timescales(init: &[u8]) -> Result<Vec<(u32, u32)>, Fmp4Error> {
    let mut saw_moov = false;
    let mut tracks = Vec::new();
    let mut iter = BoxIter::new(init, 0);

    while let Some(bx) = iter.next_box()? {
        if &bx.fourcc == b"moov" {
            saw_moov = true;
            let mut moov_iter = BoxIter::new(bx.payload, bx.payload_offset);
            while let Some(child) = moov_iter.next_box()? {
                if &child.fourcc == b"trak" {
                    tracks.push(parse_trak(child.payload, child.payload_offset)?);
                }
            }
        }
    }

    if !saw_moov {
        return Err(Fmp4Error::Missing("moov"));
    }
    if tracks.is_empty() {
        return Err(Fmp4Error::Missing("trak"));
    }
    Ok(tracks)
}

/// Each track fragment's `(track_id, baseMediaDecodeTime)` from a media segment's
/// `moof/traf/(tfhd, tfdt)`, version 0 (`u32`) and 1 (`u64`).
pub fn decode_times(segment: &[u8]) -> Result<Vec<(u32, u64)>, Fmp4Error> {
    let mut saw_moof = false;
    let mut fragments = Vec::new();
    let mut iter = BoxIter::new(segment, 0);

    while let Some(bx) = iter.next_box()? {
        if &bx.fourcc == b"moof" {
            saw_moof = true;
            let mut moof_iter = BoxIter::new(bx.payload, bx.payload_offset);
            while let Some(child) = moof_iter.next_box()? {
                if &child.fourcc == b"traf" {
                    fragments.push(parse_traf(child.payload, child.payload_offset)?);
                }
            }
        }
    }

    if !saw_moof {
        return Err(Fmp4Error::Missing("moof"));
    }
    if fragments.is_empty() {
        return Err(Fmp4Error::Missing("traf"));
    }
    Ok(fragments)
}

/// Decode time in milliseconds given a timescale (saturating, rounded down).
pub fn to_ms(decode_time: u64, timescale: u32) -> u64 {
    if timescale == 0 {
        return 0;
    }
    let ms = u128::from(decode_time).saturating_mul(1_000) / u128::from(timescale);
    u64::try_from(ms).unwrap_or(u64::MAX)
}

fn parse_trak(payload: &[u8], base_offset: usize) -> Result<(u32, u32), Fmp4Error> {
    let mut track_id: Option<u32> = None;
    let mut saw_mdia = false;
    let mut timescale: Option<u32> = None;
    let mut iter = BoxIter::new(payload, base_offset);

    while let Some(bx) = iter.next_box()? {
        match &bx.fourcc {
            b"tkhd" => {
                track_id = Some(parse_tkhd_or_mdhd(bx.payload, bx.at, "tkhd")?);
            }
            b"mdia" => {
                saw_mdia = true;
                let mut mdia_iter = BoxIter::new(bx.payload, bx.payload_offset);
                while let Some(mdia_child) = mdia_iter.next_box()? {
                    if &mdia_child.fourcc == b"mdhd" {
                        timescale = Some(parse_tkhd_or_mdhd(
                            mdia_child.payload,
                            mdia_child.at,
                            "mdhd",
                        )?);
                    }
                }
            }
            _ => {}
        }
    }

    let Some(track_id) = track_id else {
        return Err(Fmp4Error::Missing("tkhd"));
    };
    if !saw_mdia {
        return Err(Fmp4Error::Missing("mdia"));
    }
    let Some(timescale) = timescale else {
        return Err(Fmp4Error::Missing("mdhd"));
    };
    Ok((track_id, timescale))
}

fn parse_tkhd_or_mdhd(
    payload: &[u8],
    box_at: usize,
    box_name: &'static str,
) -> Result<u32, Fmp4Error> {
    let version = *payload.first().ok_or(Fmp4Error::Truncated { at: box_at })?;
    match version {
        0 => read_u32_be(payload, 12, box_at),
        1 => read_u32_be(payload, 20, box_at),
        version => Err(Fmp4Error::UnsupportedVersion {
            name: box_name,
            version,
        }),
    }
}

fn parse_traf(payload: &[u8], base_offset: usize) -> Result<(u32, u64), Fmp4Error> {
    let mut track_id: Option<u32> = None;
    let mut decode_time: Option<u64> = None;
    let mut iter = BoxIter::new(payload, base_offset);

    while let Some(bx) = iter.next_box()? {
        match &bx.fourcc {
            b"tfhd" => {
                track_id = Some(read_u32_be(bx.payload, 4, bx.at)?);
            }
            b"tfdt" => {
                decode_time = Some(parse_tfdt(bx.payload, bx.at)?);
            }
            _ => {}
        }
    }

    let Some(track_id) = track_id else {
        return Err(Fmp4Error::Missing("tfhd"));
    };
    let Some(decode_time) = decode_time else {
        return Err(Fmp4Error::Missing("tfdt"));
    };
    Ok((track_id, decode_time))
}

fn parse_tfdt(payload: &[u8], box_at: usize) -> Result<u64, Fmp4Error> {
    let version = *payload.first().ok_or(Fmp4Error::Truncated { at: box_at })?;
    match version {
        0 => Ok(u64::from(read_u32_be(payload, 4, box_at)?)),
        1 => read_u64_be(payload, 4, box_at),
        version => Err(Fmp4Error::UnsupportedVersion {
            name: "tfdt",
            version,
        }),
    }
}

fn read_u32_be(slice: &[u8], offset: usize, err_at: usize) -> Result<u32, Fmp4Error> {
    let bytes: [u8; 4] = slice
        .get(offset..offset.saturating_add(4))
        .and_then(|s| s.try_into().ok())
        .ok_or(Fmp4Error::Truncated { at: err_at })?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64_be(slice: &[u8], offset: usize, err_at: usize) -> Result<u64, Fmp4Error> {
    let bytes: [u8; 8] = slice
        .get(offset..offset.saturating_add(8))
        .and_then(|s| s.try_into().ok())
        .ok_or(Fmp4Error::Truncated { at: err_at })?;
    Ok(u64::from_be_bytes(bytes))
}

fn read_fourcc(slice: &[u8], offset: usize, err_at: usize) -> Result<[u8; 4], Fmp4Error> {
    slice
        .get(offset..offset.saturating_add(4))
        .and_then(|s| s.try_into().ok())
        .ok_or(Fmp4Error::Truncated { at: err_at })
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected parse results is the intended failure mode in tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use proptest::prelude::*;

    use super::{decode_times, timescales, to_ms, Fmp4Error};

    fn bx(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + payload.len()).unwrap();
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(payload);
        out
    }

    fn bx_large(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (16 + payload.len()) as u64;
        let mut out = Vec::with_capacity(16 + payload.len());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn bx_size0(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(payload);
        out
    }

    fn tkhd_v0(track_id: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 16];
        payload[12..16].copy_from_slice(&track_id.to_be_bytes());
        bx(b"tkhd", &payload)
    }

    fn tkhd_v1(track_id: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 24];
        payload[0] = 1;
        payload[20..24].copy_from_slice(&track_id.to_be_bytes());
        bx(b"tkhd", &payload)
    }

    fn mdhd_v0(timescale: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 16];
        payload[12..16].copy_from_slice(&timescale.to_be_bytes());
        bx(b"mdhd", &payload)
    }

    fn mdhd_v1(timescale: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 24];
        payload[0] = 1;
        payload[20..24].copy_from_slice(&timescale.to_be_bytes());
        bx(b"mdhd", &payload)
    }

    fn tfhd(track_id: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 8];
        payload[4..8].copy_from_slice(&track_id.to_be_bytes());
        bx(b"tfhd", &payload)
    }

    fn tfdt_v0(decode_time: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 8];
        payload[4..8].copy_from_slice(&decode_time.to_be_bytes());
        bx(b"tfdt", &payload)
    }

    fn tfdt_v1(decode_time: u64) -> Vec<u8> {
        let mut payload = vec![0u8; 12];
        payload[0] = 1;
        payload[4..12].copy_from_slice(&decode_time.to_be_bytes());
        bx(b"tfdt", &payload)
    }

    #[test]
    fn parses_init_with_two_traks_video_90000_and_audio_48000() {
        let mut trak1_body = tkhd_v0(1);
        trak1_body.extend_from_slice(&bx(b"mdia", &mdhd_v0(90_000)));
        let trak1 = bx(b"trak", &trak1_body);

        let mut trak2_body = tkhd_v1(2);
        trak2_body.extend_from_slice(&bx(b"mdia", &mdhd_v1(48_000)));
        let trak2 = bx(b"trak", &trak2_body);

        let mut moov_body = bx(b"mvhd", &[0u8; 16]);
        moov_body.extend_from_slice(&trak1);
        moov_body.extend_from_slice(&trak2);

        let mut init = bx(b"ftyp", b"isom\x00\x00\x02\x00");
        init.extend_from_slice(&bx(b"moov", &moov_body));

        assert_eq!(timescales(&init).unwrap(), vec![(1, 90_000), (2, 48_000)]);
    }

    #[test]
    fn parses_media_segment_with_two_trafs_tfdt_v1_and_v0() {
        let mut traf1_body = tfhd(1);
        traf1_body.extend_from_slice(&tfdt_v1(5_400_000_000));
        let traf1 = bx(b"traf", &traf1_body);

        let mut traf2_body = tfhd(2);
        traf2_body.extend_from_slice(&tfdt_v0(288_000));
        let traf2 = bx(b"traf", &traf2_body);

        let mut moof_body = bx(b"mfhd", &[0u8; 8]);
        moof_body.extend_from_slice(&traf1);
        moof_body.extend_from_slice(&traf2);

        let mut seg = bx(b"styp", b"msdh\x00\x00\x00\x00");
        seg.extend_from_slice(&bx(b"moof", &moof_body));
        seg.extend_from_slice(&bx(b"mdat", &[0xaa; 12]));

        assert_eq!(
            decode_times(&seg).unwrap(),
            vec![(1, 5_400_000_000), (2, 288_000)]
        );
    }

    #[test]
    fn handles_largesize_and_size_zero_boxes() {
        let mut trak_body = tkhd_v0(1);
        trak_body.extend_from_slice(&bx_large(b"mdia", &mdhd_v0(90_000)));
        let moov = bx_size0(b"moov", &bx_large(b"trak", &trak_body));
        assert_eq!(timescales(&moov).unwrap(), vec![(1, 90_000)]);

        let mut traf_body = tfhd(1);
        traf_body.extend_from_slice(&tfdt_v0(90_000));
        let moof = bx_size0(b"moof", &bx_large(b"traf", &traf_body));
        assert_eq!(decode_times(&moof).unwrap(), vec![(1, 90_000)]);
    }

    #[test]
    fn truncated_input_returns_truncated_error() {
        assert_eq!(timescales(&[0, 0, 0]), Err(Fmp4Error::Truncated { at: 0 }));

        let mut claimed = Vec::new();
        claimed.extend_from_slice(&100u32.to_be_bytes());
        claimed.extend_from_slice(b"moov");
        claimed.extend_from_slice(&[0u8; 4]);
        assert_eq!(timescales(&claimed), Err(Fmp4Error::Truncated { at: 0 }));

        let mut short_large = Vec::new();
        short_large.extend_from_slice(&1u32.to_be_bytes());
        short_large.extend_from_slice(b"moof");
        short_large.extend_from_slice(&[0u8; 4]);
        assert_eq!(
            decode_times(&short_large),
            Err(Fmp4Error::Truncated { at: 0 })
        );
    }

    #[test]
    fn box_claiming_size_3_returns_bad_box_size() {
        let mut bad = Vec::new();
        bad.extend_from_slice(&3u32.to_be_bytes());
        bad.extend_from_slice(b"moov");
        assert_eq!(
            timescales(&bad),
            Err(Fmp4Error::BadBoxSize { at: 0, size: 3 })
        );
        assert_eq!(
            decode_times(&bad),
            Err(Fmp4Error::BadBoxSize { at: 0, size: 3 })
        );

        let mut bad_large = Vec::new();
        bad_large.extend_from_slice(&1u32.to_be_bytes());
        bad_large.extend_from_slice(b"moov");
        bad_large.extend_from_slice(&12u64.to_be_bytes());
        assert_eq!(
            timescales(&bad_large),
            Err(Fmp4Error::BadBoxSize { at: 0, size: 12 })
        );
    }

    #[test]
    fn missing_boxes_report_named_missing_error() {
        let only_ftyp = bx(b"ftyp", b"isom");
        assert_eq!(timescales(&only_ftyp), Err(Fmp4Error::Missing("moov")));
        assert_eq!(decode_times(&only_ftyp), Err(Fmp4Error::Missing("moof")));

        let empty_moov = bx(b"moov", &[]);
        assert_eq!(timescales(&empty_moov), Err(Fmp4Error::Missing("trak")));

        let empty_moof = bx(b"moof", &[]);
        assert_eq!(decode_times(&empty_moof), Err(Fmp4Error::Missing("traf")));
    }

    #[test]
    fn to_ms_converts_and_rounds_down_with_saturation() {
        assert_eq!(to_ms(90_000, 90_000), 1_000);
        assert_eq!(to_ms(90_089, 90_000), 1_000);
        assert_eq!(to_ms(288_000, 48_000), 6_000);
        assert_eq!(to_ms(123_456, 0), 0);
        assert_eq!(to_ms(u64::MAX, 1), u64::MAX);
    }

    proptest! {
        #[test]
        fn timescales_and_decode_times_never_panic_on_arbitrary_bytes(
            bytes in prop::collection::vec(any::<u8>(), 0..512),
        ) {
            let _ = timescales(&bytes);
            let _ = decode_times(&bytes);
        }
    }
}
