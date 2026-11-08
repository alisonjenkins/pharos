//! Length-prefixed (ISO/IEC 14496-15) H.264 and HEVC to the Annex B byte
//! stream that Android's `MediaCodec` takes as input and as `csd-*` buffers.

use std::fmt;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// A malformed decoder configuration record or sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnexBError {
    Truncated {
        at: usize,
    },
    /// `configurationVersion` is not 1.
    UnsupportedVersion(u8),
    /// A NAL length field of a size the record does not allow (1, 2 or 4).
    BadLengthSize(u8),
}

impl fmt::Display for AnnexBError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { at } => write!(f, "NAL data truncated at byte offset {at}"),
            Self::UnsupportedVersion(v) => {
                write!(f, "unsupported decoder configuration record version {v}")
            }
            Self::BadLengthSize(n) => write!(f, "invalid NAL length field size {n}"),
        }
    }
}

impl std::error::Error for AnnexBError {}

/// The parameter sets of an `avcC` or `hvcC` record and the size of the
/// length field before each NAL unit in the track's samples.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParameterSets {
    pub length_size: u8,
    /// HEVC only.
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl ParameterSets {
    /// `csd-0` and `csd-1` for an H.264 `MediaFormat`: the SPSs, then the PPSs.
    pub fn avc_csd(&self) -> (Vec<u8>, Vec<u8>) {
        (join(&self.sps), join(&self.pps))
    }

    /// `csd-0` for an HEVC `MediaFormat`: the VPSs, SPSs and PPSs in one buffer.
    pub fn hevc_csd(&self) -> Vec<u8> {
        let mut out = join(&self.vps);
        out.extend(join(&self.sps));
        out.extend(join(&self.pps));
        out
    }
}

fn join(nalus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nalus.iter().map(|n| n.len().saturating_add(4)).sum());
    for n in nalus {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(n);
    }
    out
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, AnnexBError> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or(AnnexBError::Truncated { at: self.pos })?;
        self.pos = self.pos.saturating_add(1);
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16, AnnexBError> {
        Ok(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], AnnexBError> {
        let end = self.pos.saturating_add(len);
        let out = self
            .buf
            .get(self.pos..end)
            .ok_or(AnnexBError::Truncated { at: self.pos })?;
        self.pos = end;
        Ok(out)
    }

    fn skip(&mut self, len: usize) -> Result<(), AnnexBError> {
        self.take(len).map(|_| ())
    }

    fn nalus(&mut self, count: usize, out: &mut Vec<Vec<u8>>) -> Result<(), AnnexBError> {
        for _ in 0..count {
            let len = usize::from(self.u16()?);
            out.push(self.take(len)?.to_vec());
        }
        Ok(())
    }
}

fn length_size(byte: u8) -> Result<u8, AnnexBError> {
    match (byte & 0x03).saturating_add(1) {
        3 => Err(AnnexBError::BadLengthSize(3)),
        n => Ok(n),
    }
}

/// The SPSs and PPSs of an AVCDecoderConfigurationRecord (`avcC` payload).
pub fn avc_parameter_sets(avcc: &[u8]) -> Result<ParameterSets, AnnexBError> {
    let mut r = Reader { buf: avcc, pos: 0 };
    let version = r.u8()?;
    if version != 1 {
        return Err(AnnexBError::UnsupportedVersion(version));
    }
    // AVCProfileIndication, profile_compatibility, AVCLevelIndication.
    r.skip(3)?;
    let mut sets = ParameterSets {
        length_size: length_size(r.u8()?)?,
        ..Default::default()
    };
    let sps = usize::from(r.u8()? & 0x1f);
    r.nalus(sps, &mut sets.sps)?;
    let pps = usize::from(r.u8()?);
    r.nalus(pps, &mut sets.pps)?;
    Ok(sets)
}

const HEVC_VPS: u8 = 32;
const HEVC_SPS: u8 = 33;
const HEVC_PPS: u8 = 34;

/// The VPSs, SPSs and PPSs of an HEVCDecoderConfigurationRecord (`hvcC`
/// payload). SEI arrays in the record are left out.
pub fn hevc_parameter_sets(hvcc: &[u8]) -> Result<ParameterSets, AnnexBError> {
    let mut r = Reader { buf: hvcc, pos: 0 };
    let version = r.u8()?;
    if version != 1 {
        return Err(AnnexBError::UnsupportedVersion(version));
    }
    // Profile, tier, level, segmentation, parallelism, chroma, bit depths, frame rate.
    r.skip(20)?;
    let mut sets = ParameterSets {
        length_size: length_size(r.u8()?)?,
        ..Default::default()
    };
    let arrays = r.u8()?;
    for _ in 0..arrays {
        let nal_type = r.u8()? & 0x3f;
        let count = usize::from(r.u16()?);
        let mut nalus = Vec::new();
        r.nalus(count, &mut nalus)?;
        match nal_type {
            HEVC_VPS => sets.vps.extend(nalus),
            HEVC_SPS => sets.sps.extend(nalus),
            HEVC_PPS => sets.pps.extend(nalus),
            _ => {}
        }
    }
    Ok(sets)
}

/// Appends `sample`, NAL units each behind a `length_size`-byte big-endian
/// length, to `out` as Annex B (a start code before each NAL unit).
pub fn to_annex_b(sample: &[u8], length_size: u8, out: &mut Vec<u8>) -> Result<(), AnnexBError> {
    if !matches!(length_size, 1 | 2 | 4) {
        return Err(AnnexBError::BadLengthSize(length_size));
    }
    let width = usize::from(length_size);
    out.reserve(sample.len());
    let mut pos = 0usize;
    while pos < sample.len() {
        let field = sample
            .get(pos..pos.saturating_add(width))
            .ok_or(AnnexBError::Truncated { at: pos })?;
        let len = field
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        let start = pos.saturating_add(width);
        let nalu = sample
            .get(start..start.saturating_add(len))
            .ok_or(AnnexBError::Truncated { at: pos })?;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nalu);
        pos = start.saturating_add(len);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected parse results is the intended failure mode in tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use proptest::prelude::*;

    use super::{avc_parameter_sets, hevc_parameter_sets, to_annex_b, AnnexBError};
    use crate::fmp4::{sample_entries, samples, track_defaults, CodecConfig};

    #[test]
    fn splits_an_avcc_record_into_sps_and_pps_csd() {
        let avcc = [
            0x01, 0x64, 0x00, 0x0a, 0xff, 0xe1, 0x00, 0x03, 0x67, 0xaa, 0xbb, 0x01, 0x00, 0x02,
            0x68, 0xcc,
        ];
        let sets = avc_parameter_sets(&avcc).unwrap();
        assert_eq!(sets.length_size, 4);
        assert_eq!(
            sets.avc_csd(),
            (
                vec![0, 0, 0, 1, 0x67, 0xaa, 0xbb],
                vec![0, 0, 0, 1, 0x68, 0xcc]
            )
        );
    }

    #[test]
    fn rewrites_length_prefixes_as_start_codes() {
        let sample = [0, 0, 0, 2, 0x65, 0x01, 0, 0, 0, 1, 0x06];
        let mut out = Vec::new();
        to_annex_b(&sample, 4, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0x01, 0, 0, 0, 1, 0x06]);

        let mut out = Vec::new();
        to_annex_b(&[0, 1, 0x41], 2, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn a_nal_longer_than_its_sample_is_refused() {
        let mut out = Vec::new();
        assert_eq!(
            to_annex_b(&[0, 0, 0, 9, 0x65], 4, &mut out),
            Err(AnnexBError::Truncated { at: 0 })
        );
        assert_eq!(
            to_annex_b(&[], 3, &mut out),
            Err(AnnexBError::BadLengthSize(3))
        );
    }

    /// The ffmpeg fixtures: the first H.264 sample becomes an IDR picture, and
    /// the HEVC record carries one VPS, SPS and PPS of the right NAL types.
    #[test]
    fn converts_the_ffmpeg_fixtures() {
        let init = include_bytes!("../testdata/init.mp4");
        let seg = include_bytes!("../testdata/seg0.m4s");
        let entries = sample_entries(init).unwrap();
        let Some(CodecConfig::Avc(avcc)) = &entries[0].config else {
            panic!("{:?}", entries[0].config);
        };
        let sets = avc_parameter_sets(avcc).unwrap();
        assert_eq!((sets.sps.len(), sets.pps.len()), (1, 1));
        assert_eq!(sets.sps[0][0] & 0x1f, 7);
        assert_eq!(sets.pps[0][0] & 0x1f, 8);

        let first = samples(seg, &track_defaults(init).unwrap()).unwrap()[0];
        let mut out = Vec::new();
        to_annex_b(first.data(seg).unwrap(), sets.length_size, &mut out).unwrap();
        let types: Vec<u8> = out
            .windows(5)
            .filter(|w| w[..4] == [0, 0, 0, 1])
            .map(|w| w[4] & 0x1f)
            .collect();
        assert!(types.contains(&5), "no IDR slice in {types:?}");

        let entries = sample_entries(include_bytes!("../testdata/init-hevc-opus.mp4")).unwrap();
        let Some(CodecConfig::Hevc(hvcc)) = &entries[0].config else {
            panic!("{:?}", entries[0].config);
        };
        let sets = hevc_parameter_sets(hvcc).unwrap();
        assert_eq!((sets.vps.len(), sets.sps.len(), sets.pps.len()), (1, 1, 1));
        assert_eq!(sets.length_size, 4);
        let csd = sets.hevc_csd();
        let types: Vec<u8> = csd
            .windows(5)
            .filter(|w| w[..4] == [0, 0, 0, 1])
            .map(|w| (w[4] >> 1) & 0x3f)
            .collect();
        assert_eq!(types, [32, 33, 34]);
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = avc_parameter_sets(&bytes);
            let _ = hevc_parameter_sets(&bytes);
            let mut out = Vec::new();
            let _ = to_annex_b(&bytes, 4, &mut out);
        }
    }
}
