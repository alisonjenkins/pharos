//! Read-only, bounds-checked ISO BMFF (fMP4) box inspection for init and media segments.

use std::fmt;

/// Structural or bounds failure when inspecting an fMP4 segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fmp4Error {
    Truncated {
        at: usize,
    },
    BadBoxSize {
        at: usize,
        size: u64,
    },
    Missing(&'static str),
    UnsupportedVersion {
        name: &'static str,
        version: u8,
    },
    /// A `trun` declares more samples than a media segment can plausibly hold.
    TooManySamples {
        at: usize,
        count: u32,
    },
    /// A sample's bytes, from its run's data offset, fall outside the segment.
    SampleOutOfRange {
        track_id: u32,
        offset: i128,
        size: u32,
    },
    /// A sample field is absent from its `trun`, its `tfhd` and the track's `trex`.
    NoDefault {
        track_id: u32,
        field: &'static str,
    },
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
            Self::TooManySamples { at, count } => {
                write!(
                    f,
                    "trun at byte offset {at} declares {count} samples, over {MAX_TRUN_SAMPLES}"
                )
            }
            Self::SampleOutOfRange {
                track_id,
                offset,
                size,
            } => write!(
                f,
                "track {track_id} sample of {size} bytes at offset {offset} is outside the segment"
            ),
            Self::NoDefault { track_id, field } => {
                write!(
                    f,
                    "track {track_id} has no sample {field} in trun, tfhd or trex"
                )
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

/// What a track carries, from its `hdlr` and sample entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video {
        width: u16,
        height: u16,
    },
    Audio {
        channels: u16,
        sample_rate: u32,
    },
    /// Any other handler (subtitles, metadata), by its `hdlr` type.
    Other([u8; 4]),
}

/// A sample entry's decoder configuration, as the codec's own box carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecConfig {
    /// `avcC` payload (AVCDecoderConfigurationRecord).
    Avc(Vec<u8>),
    /// `hvcC` payload (HEVCDecoderConfigurationRecord).
    Hevc(Vec<u8>),
    /// `vpcC` payload.
    Vp9(Vec<u8>),
    /// `av1C` payload.
    Av1(Vec<u8>),
    /// The AudioSpecificConfig from `esds`, and its MPEG-4 object type.
    Aac { object_type: u8, asc: Vec<u8> },
    /// `dOps` payload.
    Opus(Vec<u8>),
    /// `dac3` payload.
    Ac3(Vec<u8>),
    /// `dec3` payload.
    Eac3(Vec<u8>),
}

/// One track's first sample entry from an init segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleEntry {
    pub track_id: u32,
    pub timescale: u32,
    /// The sample entry's four-character code, such as `avc1`, `hvc1` or `mp4a`.
    pub format: [u8; 4],
    pub kind: TrackKind,
    /// `None` for an entry with no codec box this reader knows, such as an
    /// encrypted `encv`.
    pub config: Option<CodecConfig>,
}

/// ISO/IEC 14496-12 VisualSampleEntry fields before its child boxes.
const VISUAL_ENTRY_LEN: usize = 78;
/// ISO/IEC 14496-12 AudioSampleEntry (version 0) fields before its child boxes.
const AUDIO_ENTRY_LEN: usize = 28;

/// Each track's first sample entry from an init segment's
/// `moov/trak/mdia/(mdhd, hdlr, minf/stbl/stsd)`.
pub fn sample_entries(init: &[u8]) -> Result<Vec<SampleEntry>, Fmp4Error> {
    let mut saw_moov = false;
    let mut out = Vec::new();
    let mut iter = BoxIter::new(init, 0);
    while let Some(bx) = iter.next_box()? {
        if &bx.fourcc != b"moov" {
            continue;
        }
        saw_moov = true;
        let mut moov_iter = BoxIter::new(bx.payload, bx.payload_offset);
        while let Some(trak) = moov_iter.next_box()? {
            if &trak.fourcc == b"trak" {
                out.push(trak_entry(&trak)?);
            }
        }
    }
    if !saw_moov {
        return Err(Fmp4Error::Missing("moov"));
    }
    if out.is_empty() {
        return Err(Fmp4Error::Missing("trak"));
    }
    Ok(out)
}

fn child<'a>(parent: &ParsedBox<'a>, fourcc: &[u8; 4]) -> Result<Option<ParsedBox<'a>>, Fmp4Error> {
    let mut iter = BoxIter::new(parent.payload, parent.payload_offset);
    while let Some(bx) = iter.next_box()? {
        if &bx.fourcc == fourcc {
            return Ok(Some(bx));
        }
    }
    Ok(None)
}

fn require<'a>(
    parent: &ParsedBox<'a>,
    fourcc: &'static [u8; 4],
    name: &'static str,
) -> Result<ParsedBox<'a>, Fmp4Error> {
    child(parent, fourcc)?.ok_or(Fmp4Error::Missing(name))
}

fn trak_entry(trak: &ParsedBox<'_>) -> Result<SampleEntry, Fmp4Error> {
    let tkhd = require(trak, b"tkhd", "tkhd")?;
    let track_id = parse_tkhd_or_mdhd(tkhd.payload, tkhd.at, "tkhd")?;
    let mdia = require(trak, b"mdia", "mdia")?;
    let mdhd = require(&mdia, b"mdhd", "mdhd")?;
    let timescale = parse_tkhd_or_mdhd(mdhd.payload, mdhd.at, "mdhd")?;
    let hdlr = require(&mdia, b"hdlr", "hdlr")?;
    let handler = read_fourcc(hdlr.payload, 8, hdlr.at)?;
    let minf = require(&mdia, b"minf", "minf")?;
    let stbl = require(&minf, b"stbl", "stbl")?;
    let stsd = require(&stbl, b"stsd", "stsd")?;
    // version, flags and entry_count come before the entries.
    let entries = stsd
        .payload
        .get(8..)
        .ok_or(Fmp4Error::Truncated { at: stsd.at })?;
    let entry = BoxIter::new(entries, stsd.payload_offset.saturating_add(8))
        .next_box()?
        .ok_or(Fmp4Error::Missing("sample entry"))?;

    let (kind, children_at) = match &handler {
        b"vide" => (
            TrackKind::Video {
                width: read_u16_be(entry.payload, 24, entry.at)?,
                height: read_u16_be(entry.payload, 26, entry.at)?,
            },
            VISUAL_ENTRY_LEN,
        ),
        b"soun" => (
            TrackKind::Audio {
                channels: read_u16_be(entry.payload, 16, entry.at)?,
                // 16.16 fixed point; the integer half is the rate in Hz.
                sample_rate: read_u32_be(entry.payload, 24, entry.at)? >> 16,
            },
            AUDIO_ENTRY_LEN,
        ),
        other => (TrackKind::Other(*other), entry.payload.len()),
    };
    let mut config = None;
    if let Some(children) = entry.payload.get(children_at..) {
        let mut iter = BoxIter::new(children, entry.payload_offset.saturating_add(children_at));
        while let Some(bx) = iter.next_box()? {
            let data = bx.payload.to_vec();
            config = match &bx.fourcc {
                b"avcC" => Some(CodecConfig::Avc(data)),
                b"hvcC" => Some(CodecConfig::Hevc(data)),
                b"vpcC" => Some(CodecConfig::Vp9(data)),
                b"av1C" => Some(CodecConfig::Av1(data)),
                b"dOps" => Some(CodecConfig::Opus(data)),
                b"dac3" => Some(CodecConfig::Ac3(data)),
                b"dec3" => Some(CodecConfig::Eac3(data)),
                b"esds" => Some(parse_esds(bx.payload, bx.at)?),
                _ => continue,
            };
            break;
        }
    }
    Ok(SampleEntry {
        track_id,
        timescale,
        format: entry.fourcc,
        kind,
        config,
    })
}

/// The AudioSpecificConfig inside an `esds` box's ES_Descriptor (ISO/IEC 14496-1).
fn parse_esds(payload: &[u8], at: usize) -> Result<CodecConfig, Fmp4Error> {
    let es = payload.get(4..).ok_or(Fmp4Error::Truncated { at })?;
    let es = descriptor(es, 0x03, at)?;
    let flags = *es.get(2).ok_or(Fmp4Error::Truncated { at })?;
    let mut pos = 3usize;
    if flags & 0x80 != 0 {
        pos = pos.saturating_add(2);
    }
    if flags & 0x40 != 0 {
        let url_len = *es.get(pos).ok_or(Fmp4Error::Truncated { at })?;
        pos = pos.saturating_add(1).saturating_add(usize::from(url_len));
    }
    if flags & 0x20 != 0 {
        pos = pos.saturating_add(2);
    }
    let dcd = descriptor(es.get(pos..).ok_or(Fmp4Error::Truncated { at })?, 0x04, at)?;
    let object_type = *dcd.first().ok_or(Fmp4Error::Truncated { at })?;
    // objectTypeIndication, streamType, bufferSizeDB (3), maxBitrate, avgBitrate.
    let dsi = descriptor(dcd.get(13..).ok_or(Fmp4Error::Truncated { at })?, 0x05, at)?;
    Ok(CodecConfig::Aac {
        object_type,
        asc: dsi.to_vec(),
    })
}

/// The body of an MPEG-4 descriptor with tag `tag` at the start of `buf`; its
/// size is up to four 7-bit groups.
fn descriptor(buf: &[u8], tag: u8, at: usize) -> Result<&[u8], Fmp4Error> {
    if buf.first() != Some(&tag) {
        return Err(Fmp4Error::Missing(match tag {
            0x03 => "ES_Descriptor",
            0x04 => "DecoderConfigDescriptor",
            _ => "DecoderSpecificInfo",
        }));
    }
    let mut len = 0usize;
    let mut pos = 1usize;
    for _ in 0..4 {
        let b = *buf.get(pos).ok_or(Fmp4Error::Truncated { at })?;
        pos = pos.saturating_add(1);
        len = (len << 7) | usize::from(b & 0x7f);
        if b & 0x80 == 0 {
            break;
        }
    }
    buf.get(pos..pos.saturating_add(len))
        .ok_or(Fmp4Error::Truncated { at })
}

fn read_u16_be(slice: &[u8], offset: usize, err_at: usize) -> Result<u16, Fmp4Error> {
    let bytes: [u8; 2] = slice
        .get(offset..offset.saturating_add(2))
        .and_then(|s| s.try_into().ok())
        .ok_or(Fmp4Error::Truncated { at: err_at })?;
    Ok(u16::from_be_bytes(bytes))
}

/// A 10 s segment at 120 fps is 1200 video samples, and 48 kHz AAC about 470;
/// a count far past that is a corrupt or hostile `trun`, not a real fragment.
const MAX_TRUN_SAMPLES: u32 = 65_536;

/// ISO/IEC 14496-12 sample flags: `sample_is_non_sync_sample`.
const SAMPLE_IS_NON_SYNC: u32 = 0x0001_0000;

/// One track's fragment defaults from an init segment's `moov/mvex/trex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackDefaults {
    pub track_id: u32,
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}

/// One coded sample (a video access unit or an audio frame) in a media segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub track_id: u32,
    /// Decode time in the track's timescale.
    pub decode_time: u64,
    /// Presentation minus decode time, in the track's timescale.
    pub composition_offset: i64,
    pub duration: u32,
    /// Byte offset of the sample's data from the start of the segment.
    pub offset: usize,
    pub size: u32,
    /// A sync sample (key frame) a decoder can start from.
    pub sync: bool,
}

impl Sample {
    /// Presentation time in the track's timescale, clamped at zero.
    pub fn presentation_time(&self) -> u64 {
        let pts = i128::from(self.decode_time).saturating_add(i128::from(self.composition_offset));
        u64::try_from(pts.max(0)).unwrap_or(u64::MAX)
    }

    /// The sample's bytes within the segment it was read from.
    pub fn data<'a>(&self, segment: &'a [u8]) -> Option<&'a [u8]> {
        let size = usize::try_from(self.size).ok()?;
        segment.get(self.offset..self.offset.checked_add(size)?)
    }
}

/// Each track's `trex` defaults from an init segment's `moov/mvex`. A track with
/// no `trex` is left out; its fragments must then carry every field themselves.
pub fn track_defaults(init: &[u8]) -> Result<Vec<TrackDefaults>, Fmp4Error> {
    let mut saw_moov = false;
    let mut out = Vec::new();
    let mut iter = BoxIter::new(init, 0);
    while let Some(bx) = iter.next_box()? {
        if &bx.fourcc != b"moov" {
            continue;
        }
        saw_moov = true;
        let mut moov_iter = BoxIter::new(bx.payload, bx.payload_offset);
        while let Some(child) = moov_iter.next_box()? {
            if &child.fourcc != b"mvex" {
                continue;
            }
            let mut mvex_iter = BoxIter::new(child.payload, child.payload_offset);
            while let Some(trex) = mvex_iter.next_box()? {
                if &trex.fourcc == b"trex" {
                    out.push(TrackDefaults {
                        track_id: read_u32_be(trex.payload, 4, trex.at)?,
                        duration: read_u32_be(trex.payload, 12, trex.at)?,
                        size: read_u32_be(trex.payload, 16, trex.at)?,
                        flags: read_u32_be(trex.payload, 20, trex.at)?,
                    });
                }
            }
        }
    }
    if !saw_moov {
        return Err(Fmp4Error::Missing("moov"));
    }
    Ok(out)
}

/// Every sample of every track fragment in a media segment, in file order, from
/// `moof/traf/(tfhd, tfdt, trun)`, with `defaults` from [`track_defaults`].
pub fn samples(segment: &[u8], defaults: &[TrackDefaults]) -> Result<Vec<Sample>, Fmp4Error> {
    let mut saw_moof = false;
    let mut out = Vec::new();
    let mut iter = BoxIter::new(segment, 0);
    while let Some(bx) = iter.next_box()? {
        if &bx.fourcc != b"moof" {
            continue;
        }
        saw_moof = true;
        // With no base-data-offset and no default-base-is-moof, the first traf's
        // data starts at the moof and each later traf's where the previous ended.
        let mut implicit_base = bx.at;
        let mut moof_iter = BoxIter::new(bx.payload, bx.payload_offset);
        while let Some(traf) = moof_iter.next_box()? {
            if &traf.fourcc == b"traf" {
                implicit_base =
                    traf_samples(segment, &traf, bx.at, implicit_base, defaults, &mut out)?;
            }
        }
    }
    if !saw_moof {
        return Err(Fmp4Error::Missing("moof"));
    }
    Ok(out)
}

#[derive(Default)]
struct FragmentHeader {
    track_id: u32,
    base_data_offset: Option<u64>,
    default_base_is_moof: bool,
    duration: Option<u32>,
    size: Option<u32>,
    flags: Option<u32>,
}

fn parse_tfhd(payload: &[u8], at: usize) -> Result<FragmentHeader, Fmp4Error> {
    let tf_flags = read_u32_be(payload, 0, at)? & 0x00ff_ffff;
    let mut h = FragmentHeader {
        track_id: read_u32_be(payload, 4, at)?,
        default_base_is_moof: tf_flags & 0x02_0000 != 0,
        ..FragmentHeader::default()
    };
    let mut pos = 8usize;
    if tf_flags & 0x01 != 0 {
        h.base_data_offset = Some(read_u64_be(payload, pos, at)?);
        pos = pos.saturating_add(8);
    }
    if tf_flags & 0x02 != 0 {
        pos = pos.saturating_add(4);
    }
    if tf_flags & 0x08 != 0 {
        h.duration = Some(read_u32_be(payload, pos, at)?);
        pos = pos.saturating_add(4);
    }
    if tf_flags & 0x10 != 0 {
        h.size = Some(read_u32_be(payload, pos, at)?);
        pos = pos.saturating_add(4);
    }
    if tf_flags & 0x20 != 0 {
        h.flags = Some(read_u32_be(payload, pos, at)?);
    }
    Ok(h)
}

/// Appends one traf's samples to `out` and returns where its data ended, the
/// implicit base for the next traf in the same moof.
fn traf_samples(
    segment: &[u8],
    traf: &ParsedBox<'_>,
    moof_at: usize,
    implicit_base: usize,
    defaults: &[TrackDefaults],
    out: &mut Vec<Sample>,
) -> Result<usize, Fmp4Error> {
    let mut header: Option<FragmentHeader> = None;
    let mut decode_time: Option<u64> = None;
    let mut iter = BoxIter::new(traf.payload, traf.payload_offset);
    while let Some(bx) = iter.next_box()? {
        match &bx.fourcc {
            b"tfhd" => header = Some(parse_tfhd(bx.payload, bx.at)?),
            b"tfdt" => decode_time = Some(parse_tfdt(bx.payload, bx.at)?),
            _ => {}
        }
    }
    let header = header.ok_or(Fmp4Error::Missing("tfhd"))?;
    let mut decode_time = decode_time.ok_or(Fmp4Error::Missing("tfdt"))?;
    let trex = defaults.iter().find(|d| d.track_id == header.track_id);
    let base: i128 = match header.base_data_offset {
        Some(offset) => i128::from(offset),
        None if header.default_base_is_moof => moof_at as i128,
        None => implicit_base as i128,
    };

    // A trun with no data offset continues where the previous one ended.
    let mut next_data = base;
    let mut iter = BoxIter::new(traf.payload, traf.payload_offset);
    while let Some(bx) = iter.next_box()? {
        if &bx.fourcc != b"trun" {
            continue;
        }
        let run = Run::parse(bx.payload, bx.at)?;
        let mut cursor = match run.data_offset {
            Some(rel) => base.saturating_add(i128::from(rel)),
            None => next_data,
        };
        let mut pos = run.entries_at;
        for i in 0..run.count {
            let mut field = |present: bool| -> Result<Option<u32>, Fmp4Error> {
                if !present {
                    return Ok(None);
                }
                let v = read_u32_be(bx.payload, pos, bx.at)?;
                pos = pos.saturating_add(4);
                Ok(Some(v))
            };
            let duration = field(run.flags & 0x100 != 0)?;
            let size = field(run.flags & 0x200 != 0)?;
            let flags = field(run.flags & 0x400 != 0)?;
            let cto = field(run.flags & 0x800 != 0)?;

            let track_id = header.track_id;
            let pick = |own: Option<u32>, tfhd: Option<u32>, trex: Option<u32>, name| {
                own.or(tfhd).or(trex).ok_or(Fmp4Error::NoDefault {
                    track_id,
                    field: name,
                })
            };
            let duration = pick(
                duration,
                header.duration,
                trex.map(|t| t.duration),
                "duration",
            )?;
            let size = pick(size, header.size, trex.map(|t| t.size), "size")?;
            let first_flags = if i == 0 { run.first_sample_flags } else { None };
            let flags = pick(
                first_flags.or(flags),
                header.flags,
                trex.map(|t| t.flags),
                "flags",
            )?;
            let composition_offset = match cto {
                None => 0,
                // Version 1 makes the offset signed; version 0 keeps it unsigned.
                Some(v) if run.version == 1 => i64::from(v as i32),
                Some(v) => i64::from(v),
            };

            let end = cursor.saturating_add(i128::from(size));
            let offset = usize::try_from(cursor)
                .ok()
                .filter(|_| end <= segment.len() as i128)
                .ok_or(Fmp4Error::SampleOutOfRange {
                    track_id,
                    offset: cursor,
                    size,
                })?;
            out.push(Sample {
                track_id,
                decode_time,
                composition_offset,
                duration,
                offset,
                size,
                sync: flags & SAMPLE_IS_NON_SYNC == 0,
            });
            decode_time = decode_time.saturating_add(u64::from(duration));
            cursor = end;
        }
        next_data = cursor;
    }
    Ok(usize::try_from(next_data).unwrap_or(implicit_base))
}

struct Run {
    version: u8,
    flags: u32,
    count: u32,
    data_offset: Option<i32>,
    first_sample_flags: Option<u32>,
    entries_at: usize,
}

impl Run {
    fn parse(payload: &[u8], at: usize) -> Result<Self, Fmp4Error> {
        let word = read_u32_be(payload, 0, at)?;
        let version = (word >> 24) as u8;
        let flags = word & 0x00ff_ffff;
        if version > 1 {
            return Err(Fmp4Error::UnsupportedVersion {
                name: "trun",
                version,
            });
        }
        let count = read_u32_be(payload, 4, at)?;
        if count > MAX_TRUN_SAMPLES {
            return Err(Fmp4Error::TooManySamples { at, count });
        }
        let mut pos = 8usize;
        let data_offset = if flags & 0x001 != 0 {
            let v = read_u32_be(payload, pos, at)? as i32;
            pos = pos.saturating_add(4);
            Some(v)
        } else {
            None
        };
        let first_sample_flags = if flags & 0x004 != 0 {
            let v = read_u32_be(payload, pos, at)?;
            pos = pos.saturating_add(4);
            Some(v)
        } else {
            None
        };
        Ok(Self {
            version,
            flags,
            count,
            data_offset,
            first_sample_flags,
            entries_at: pos,
        })
    }
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

    use super::{
        decode_times, sample_entries, samples, timescales, to_ms, track_defaults, CodecConfig,
        Fmp4Error, TrackDefaults, TrackKind,
    };

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
            let _ = track_defaults(&bytes);
            let _ = samples(&bytes, &[]);
            let _ = sample_entries(&bytes);
        }
    }

    const NON_SYNC: u32 = 0x0001_0000;

    fn full(version: u8, flags: u32, rest: &[u8]) -> Vec<u8> {
        let mut out = ((u32::from(version) << 24) | flags).to_be_bytes().to_vec();
        out.extend_from_slice(rest);
        out
    }

    fn be(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    fn trex(track_id: u32, duration: u32, size: u32, flags: u32) -> Vec<u8> {
        bx(
            b"trex",
            &full(0, 0, &be(&[track_id, 1, duration, size, flags])),
        )
    }

    fn tfhd_flags(flags: u32, track_id: u32, fields: &[u32]) -> Vec<u8> {
        let mut rest = track_id.to_be_bytes().to_vec();
        rest.extend_from_slice(&be(fields));
        bx(b"tfhd", &full(0, flags, &rest))
    }

    /// A segment of one moof (with `trafs`) and one mdat of `mdat_len` bytes, with
    /// every trun data offset patched to point `data_at` bytes into the mdat payload.
    fn segment(trafs: &[Vec<u8>], mdat_len: usize) -> (Vec<u8>, usize) {
        let mut moof_body = bx(b"mfhd", &[0u8; 8]);
        for t in trafs {
            moof_body.extend_from_slice(&bx(b"traf", t));
        }
        let moof = bx(b"moof", &moof_body);
        let mdat_payload_at = moof.len() + 8;
        let mut seg = moof;
        seg.extend_from_slice(&bx(b"mdat", &vec![0u8; mdat_len]));
        (seg, mdat_payload_at)
    }

    #[test]
    fn reads_trex_defaults_from_mvex() {
        let mut mvex = trex(1, 3_000, 0, NON_SYNC);
        mvex.extend_from_slice(&trex(2, 1_024, 6, 0));
        let mut moov = bx(b"mvhd", &[0u8; 16]);
        moov.extend_from_slice(&bx(b"mvex", &mvex));
        let init = bx(b"moov", &moov);
        assert_eq!(
            track_defaults(&init).unwrap(),
            vec![
                TrackDefaults {
                    track_id: 1,
                    duration: 3_000,
                    size: 0,
                    flags: NON_SYNC
                },
                TrackDefaults {
                    track_id: 2,
                    duration: 1_024,
                    size: 6,
                    flags: 0
                },
            ]
        );
    }

    #[test]
    fn video_run_takes_sizes_and_signed_offsets_from_trun_and_the_rest_from_trex() {
        // The mdat payload starts at a fixed offset from the moof, known once
        // the moof is built; build once to measure, then again with the offset.
        let traf = |data_offset: u32| {
            let mut t = tfhd_flags(0x02_0000, 1, &[]);
            t.extend_from_slice(&tfdt_v1(90_000));
            // data-offset, first-sample-flags, size and composition offset per sample.
            let mut run = be(&[3, data_offset, 0]);
            run.extend_from_slice(&be(&[100, 3_000, 40, (-3_000i32) as u32, 60, 0]));
            t.extend_from_slice(&bx(b"trun", &full(1, 0x001 | 0x004 | 0x200 | 0x800, &run)));
            t
        };
        let (probe, data_at) = segment(&[traf(0)], 200);
        let (seg, _) = segment(&[traf(u32::try_from(data_at).unwrap())], 200);
        assert_eq!(probe.len(), seg.len());
        let defaults = [TrackDefaults {
            track_id: 1,
            duration: 3_000,
            size: 0,
            flags: NON_SYNC,
        }];

        let got = samples(&seg, &defaults).unwrap();

        let want = [
            (90_000, 3_000, data_at, 100, true),
            (93_000, -3_000, data_at + 100, 40, false),
            (96_000, 0, data_at + 140, 60, false),
        ];
        assert_eq!(got.len(), want.len());
        for (s, (dts, cto, offset, size, sync)) in got.iter().zip(want) {
            assert_eq!(
                (
                    s.track_id,
                    s.decode_time,
                    s.composition_offset,
                    s.duration,
                    s.offset,
                    s.size,
                    s.sync
                ),
                (1, dts, cto, 3_000, offset, size, sync)
            );
        }
        assert_eq!(got[0].presentation_time(), 93_000);
        assert_eq!(got[1].presentation_time(), 90_000);
        assert_eq!(got[2].data(&seg).map(<[u8]>::len), Some(60));
    }

    #[test]
    fn a_trun_without_a_data_offset_continues_after_the_previous_run() {
        let traf = |data_offset: u32| {
            // tfhd default size 10 and default flags (sync).
            let mut t = tfhd_flags(0x02_0000 | 0x08 | 0x10 | 0x20, 2, &[1_024, 10, 0]);
            t.extend_from_slice(&tfdt_v0(0));
            t.extend_from_slice(&bx(b"trun", &full(0, 0x001, &be(&[2, data_offset]))));
            t.extend_from_slice(&bx(b"trun", &full(0, 0, &be(&[1]))));
            t
        };
        let (_, data_at) = segment(&[traf(0)], 30);
        let (seg, _) = segment(&[traf(u32::try_from(data_at).unwrap())], 30);

        let got = samples(&seg, &[]).unwrap();

        let offsets: Vec<_> = got
            .iter()
            .map(|s| (s.offset, s.decode_time, s.sync))
            .collect();
        assert_eq!(
            offsets,
            vec![
                (data_at, 0, true),
                (data_at + 10, 1_024, true),
                (data_at + 20, 2_048, true)
            ]
        );
    }

    #[test]
    fn an_explicit_base_data_offset_wins_over_the_moof() {
        let mut t = tfhd_flags(0x01 | 0x10 | 0x08 | 0x20, 1, &[0, 4, 1, 4, 0]);
        t.extend_from_slice(&tfdt_v0(0));
        t.extend_from_slice(&bx(b"trun", &full(0, 0, &be(&[1]))));
        let (seg, _) = segment(&[t], 8);
        // base_data_offset 4 (the u64 0, 4), default duration 1, size 4.
        let got = samples(&seg, &[]).unwrap();
        assert_eq!(
            got.iter().map(|s| (s.offset, s.size)).collect::<Vec<_>>(),
            vec![(4, 4)]
        );
    }

    #[test]
    fn a_field_with_no_default_anywhere_names_the_track_and_field() {
        let mut t = tfhd_flags(0x02_0000, 7, &[]);
        t.extend_from_slice(&tfdt_v0(0));
        t.extend_from_slice(&bx(b"trun", &full(0, 0x200, &be(&[1, 4]))));
        let (seg, _) = segment(&[t], 8);
        assert_eq!(
            samples(&seg, &[]),
            Err(Fmp4Error::NoDefault {
                track_id: 7,
                field: "duration"
            })
        );
    }

    #[test]
    fn a_sample_past_the_end_of_the_segment_is_refused() {
        let mut t = tfhd_flags(0x02_0000 | 0x08 | 0x20, 1, &[1, 0]);
        t.extend_from_slice(&tfdt_v0(0));
        t.extend_from_slice(&bx(b"trun", &full(0, 0x200, &be(&[1, 1_000_000]))));
        let (seg, _) = segment(&[t], 8);
        assert!(matches!(
            samples(&seg, &[]),
            Err(Fmp4Error::SampleOutOfRange {
                track_id: 1,
                size: 1_000_000,
                ..
            })
        ));
    }

    #[test]
    fn a_huge_sample_count_is_refused_before_reading_entries() {
        let mut t = tfhd_flags(0x02_0000, 1, &[]);
        t.extend_from_slice(&tfdt_v0(0));
        t.extend_from_slice(&bx(b"trun", &full(0, 0, &be(&[u32::MAX]))));
        let (seg, _) = segment(&[t], 8);
        assert!(matches!(
            samples(&seg, &[]),
            Err(Fmp4Error::TooManySamples {
                count: u32::MAX,
                ..
            })
        ));
    }

    #[test]
    fn a_segment_without_a_moof_is_refused() {
        assert_eq!(
            samples(&bx(b"mdat", &[0; 4]), &[]),
            Err(Fmp4Error::Missing("moof"))
        );
    }

    /// ffmpeg's own HLS fMP4 output (H.264 with B-frames at 30 fps, AAC at 48 kHz),
    /// checked against `ffprobe -show_entries packet` on the same two files.
    #[test]
    fn reads_every_sample_of_an_ffmpeg_hls_segment() {
        let init = include_bytes!("../testdata/init.mp4");
        let seg = include_bytes!("../testdata/seg0.m4s");
        let defaults = track_defaults(init).unwrap();

        let got = samples(seg, &defaults).unwrap();

        let video: Vec<_> = got.iter().filter(|s| s.track_id == 1).collect();
        let audio: Vec<_> = got.iter().filter(|s| s.track_id == 2).collect();
        let sizes: Vec<u32> = video.iter().map(|s| s.size).collect();
        assert_eq!(
            sizes,
            [
                1350, 12, 12, 12, 59, 37, 13, 27, 14, 12, 24, 14, 12, 31, 14, 12, 39, 17, 12, 13,
                14, 12, 45, 14, 12, 63, 14, 12, 14, 12,
            ]
        );
        let ctos: Vec<i64> = video.iter().map(|s| s.composition_offset).collect();
        assert_eq!(
            ctos,
            [
                1024, 2048, 512, 512, 2048, 512, 512, 2048, 512, 512, 2048, 512, 512, 2048, 512,
                512, 2048, 512, 512, 2048, 512, 512, 2048, 512, 512, 2048, 512, 512, 1536, 512,
            ]
        );
        assert_eq!(video.iter().filter(|s| s.sync).count(), 1);
        assert!(video[0].sync);
        assert_eq!(audio.len(), 45);
        assert_eq!(audio.iter().map(|s| s.size).sum::<u32>(), 3_868);
        assert!(audio.iter().all(|s| s.sync && s.duration == 1_024));
        for pair in video.windows(2) {
            assert_eq!(
                pair[1].decode_time,
                pair[0].decode_time + u64::from(pair[0].duration)
            );
        }
        // Each sample's bytes lie inside the segment's mdat.
        assert!(got.iter().all(|s| s.data(seg).is_some()));
    }

    /// The codec boxes of ffmpeg's H.264 + AAC and HEVC + Opus init segments,
    /// checked against the extradata `ffprobe -show_data` prints for each stream.
    #[test]
    fn reads_codec_config_from_ffmpeg_init_segments() {
        let init = include_bytes!("../testdata/init.mp4");
        let entries = sample_entries(init).unwrap();
        let scales = timescales(init).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.track_id, e.timescale))
                .collect::<Vec<_>>(),
            scales
        );
        assert_eq!(&entries[0].format, b"avc1");
        assert_eq!(
            entries[0].kind,
            TrackKind::Video {
                width: 64,
                height: 36
            }
        );
        let Some(CodecConfig::Avc(avcc)) = &entries[0].config else {
            panic!("{:?}", entries[0].config);
        };
        assert_eq!(
            (avcc.len(), &avcc[..4]),
            (44, &[0x01, 0x64, 0x00, 0x0a][..])
        );
        assert_eq!(&entries[1].format, b"mp4a");
        assert_eq!(
            entries[1].kind,
            TrackKind::Audio {
                channels: 1,
                sample_rate: 48_000
            }
        );
        assert_eq!(
            entries[1].config,
            Some(CodecConfig::Aac {
                object_type: 0x40,
                asc: vec![0x11, 0x88, 0x56, 0xe5, 0x00]
            })
        );

        let entries = sample_entries(include_bytes!("../testdata/init-hevc-opus.mp4")).unwrap();
        assert_eq!(&entries[0].format, b"hvc1");
        assert_eq!(
            entries[0].kind,
            TrackKind::Video {
                width: 64,
                height: 36
            }
        );
        let Some(CodecConfig::Hevc(hvcc)) = &entries[0].config else {
            panic!("{:?}", entries[0].config);
        };
        assert_eq!((hvcc.len(), &hvcc[..3]), (2_461, &[0x01, 0x02, 0x20][..]));
        assert_eq!(&entries[1].format, b"Opus");
        assert_eq!(
            entries[1].kind,
            TrackKind::Audio {
                channels: 2,
                sample_rate: 48_000
            }
        );
        // dOps: version 0, 2 channels, pre-skip 312, 48 kHz, gain 0, family 0.
        assert_eq!(
            entries[1].config,
            Some(CodecConfig::Opus(vec![
                0, 2, 0x01, 0x38, 0, 0, 0xbb, 0x80, 0, 0, 0
            ]))
        );
    }
}
