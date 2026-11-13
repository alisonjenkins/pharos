//! Pure fMP4 (ISO/IEC 14496-12) box writer for the `h264copy` HLS surface (ADR-0123).
//!
//! Writes a single-track H.264 init segment (`ftyp` + `moov`) and per-segment
//! media fragments (`moof` + `mdat`) directly from a source's `avcC` decoder
//! configuration record and demuxed video packets, without invoking libav's
//! mp4 muxer or re-encoding.

/// Canonical track ID used by `h264copy` init and media segments.
pub const COPY_TRACK_ID: u32 = 1;

const MOVIE_TIMESCALE: u32 = 1_000;
const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
const PACKED_LANG_UND: u16 = 0x55C4;
const SAMPLE_FLAGS_SYNC: u32 = 0x0200_0000;
const SAMPLE_FLAGS_NON_SYNC: u32 = 0x0101_0000;
const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x02_0000;
const TRUN_ALL_SAMPLE_FIELDS: u32 = 0x00_0F01;

/// Video track metadata required to build an `h264copy` init or media segment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CopyTrack {
    pub avcc: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub sar: Option<(u32, u32)>,
    pub timescale: u32,
}

/// One demuxed H.264 access unit in AVCC length-prefixed format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyPacket {
    pub data: Vec<u8>,
    pub pts: i64,
    pub keyframe: bool,
}

/// Failure when validating inputs or writing `h264copy` fMP4 boxes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CopyWriteError {
    #[error(
        "avcC record is not version 1 or shorter than 7 bytes (first byte {first:?}, len {len})"
    )]
    NotAvcc { first: Option<u8>, len: usize },
    #[error("timescale is zero")]
    ZeroTimescale,
    #[error("segment has no packets")]
    Empty,
    #[error("first packet is not a keyframe (pts {pts})")]
    FirstNotKeyframe { pts: i64 },
    #[error("first keyframe pts {pts} is not the segment start {start}")]
    StartMismatch { pts: i64, start: i64 },
    #[error("packet pts {pts} is before the segment start {start} (open GOP)")]
    OpenGop { pts: i64, start: i64 },
    #[error("packet pts {pts} is not before the segment end {end}")]
    PastEnd { pts: i64, end: i64 },
    #[error("two packets share pts {pts}")]
    DuplicatePts { pts: i64 },
    #[error("segment start {start} is negative")]
    NegativeStart { start: i64 },
    #[error("{what} {value} does not fit the box field")]
    Overflow { what: &'static str, value: i128 },
}

fn validate_avcc(avcc: &[u8]) -> Result<[u8; 3], CopyWriteError> {
    match *avcc {
        [1, profile, compat, level, _, _, _, ..] => Ok([profile, compat, level]),
        _ => Err(CopyWriteError::NotAvcc {
            first: avcc.first().copied(),
            len: avcc.len(),
        }),
    }
}

fn checked_u32(value: i128, what: &'static str) -> Result<u32, CopyWriteError> {
    u32::try_from(value).map_err(|_| CopyWriteError::Overflow { what, value })
}

fn checked_i32(value: i128, what: &'static str) -> Result<i32, CopyWriteError> {
    i32::try_from(value).map_err(|_| CopyWriteError::Overflow { what, value })
}

fn usize_to_i128(v: usize) -> i128 {
    i128::try_from(v).unwrap_or(i128::MAX)
}

fn write_box(
    out: &mut Vec<u8>,
    fourcc: &[u8; 4],
    body: impl FnOnce(&mut Vec<u8>) -> Result<(), CopyWriteError>,
) -> Result<(), CopyWriteError> {
    let start = out.len();
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(fourcc);
    body(out)?;
    let size = checked_u32(usize_to_i128(out.len() - start), "box size")?;
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
    Ok(())
}

fn write_full_box(
    out: &mut Vec<u8>,
    fourcc: &[u8; 4],
    version: u8,
    flags: u32,
    body: impl FnOnce(&mut Vec<u8>) -> Result<(), CopyWriteError>,
) -> Result<(), CopyWriteError> {
    write_box(out, fourcc, |out| {
        let vf = (u32::from(version) << 24) | (flags & 0x00FF_FFFF);
        out.extend_from_slice(&vf.to_be_bytes());
        body(out)
    })
}

/// Returns the RFC 6381 `codecs` string `avc1.PPCCLL` from `avcC` bytes `1..=3`, lowercase hex.
pub fn codecs_string(avcc: &[u8]) -> Result<String, CopyWriteError> {
    let [profile, compat, level] = validate_avcc(avcc)?;
    Ok(format!("avc1.{profile:02x}{compat:02x}{level:02x}"))
}

/// Builds the `ftyp` + `moov` initialization segment for `track`.
pub fn init_segment(track: &CopyTrack) -> Result<Vec<u8>, CopyWriteError> {
    validate_avcc(&track.avcc)?;
    if track.timescale == 0 {
        return Err(CopyWriteError::ZeroTimescale);
    }

    let display_w = match track.sar {
        Some((_, 0)) => {
            return Err(CopyWriteError::Overflow {
                what: "sar denominator",
                value: 0,
            });
        }
        Some((num, den)) => {
            let w = i128::from(track.width) * i128::from(num) / i128::from(den);
            u16::try_from(w).map_err(|_| CopyWriteError::Overflow {
                what: "tkhd display width",
                value: w,
            })?
        }
        None => track.width,
    };
    let tkhd_width = u32::from(display_w) << 16;
    let tkhd_height = u32::from(track.height) << 16;

    let mut out = Vec::with_capacity(512 + track.avcc.len());

    write_box(&mut out, b"ftyp", |out| {
        out.extend_from_slice(b"iso6");
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(b"iso6isomavc1mp41");
        Ok(())
    })?;

    write_box(&mut out, b"moov", |out| {
        write_full_box(out, b"mvhd", 0, 0, |out| {
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&MOVIE_TIMESCALE.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            out.extend_from_slice(&0x0100u16.to_be_bytes());
            out.extend_from_slice(&[0u8; 10]);
            for w in UNITY_MATRIX {
                out.extend_from_slice(&w.to_be_bytes());
            }
            out.extend_from_slice(&[0u8; 24]);
            out.extend_from_slice(&2u32.to_be_bytes());
            Ok(())
        })?;

        write_box(out, b"trak", |out| {
            write_full_box(out, b"tkhd", 0, 0x00_0003, |out| {
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&COPY_TRACK_ID.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&[0u8; 8]);
                out.extend_from_slice(&0u16.to_be_bytes());
                out.extend_from_slice(&0u16.to_be_bytes());
                out.extend_from_slice(&0u16.to_be_bytes());
                out.extend_from_slice(&0u16.to_be_bytes());
                for w in UNITY_MATRIX {
                    out.extend_from_slice(&w.to_be_bytes());
                }
                out.extend_from_slice(&tkhd_width.to_be_bytes());
                out.extend_from_slice(&tkhd_height.to_be_bytes());
                Ok(())
            })?;

            write_box(out, b"mdia", |out| {
                write_full_box(out, b"mdhd", 0, 0, |out| {
                    out.extend_from_slice(&0u32.to_be_bytes());
                    out.extend_from_slice(&0u32.to_be_bytes());
                    out.extend_from_slice(&track.timescale.to_be_bytes());
                    out.extend_from_slice(&0u32.to_be_bytes());
                    out.extend_from_slice(&PACKED_LANG_UND.to_be_bytes());
                    out.extend_from_slice(&0u16.to_be_bytes());
                    Ok(())
                })?;

                write_full_box(out, b"hdlr", 0, 0, |out| {
                    out.extend_from_slice(&0u32.to_be_bytes());
                    out.extend_from_slice(b"vide");
                    out.extend_from_slice(&[0u8; 12]);
                    out.extend_from_slice(b"VideoHandler\0");
                    Ok(())
                })?;

                write_box(out, b"minf", |out| {
                    write_full_box(out, b"vmhd", 0, 1, |out| {
                        out.extend_from_slice(&[0u8; 8]);
                        Ok(())
                    })?;

                    write_box(out, b"dinf", |out| {
                        write_full_box(out, b"dref", 0, 0, |out| {
                            out.extend_from_slice(&1u32.to_be_bytes());
                            write_full_box(out, b"url ", 0, 1, |_| Ok(()))
                        })
                    })?;

                    write_box(out, b"stbl", |out| {
                        write_full_box(out, b"stsd", 0, 0, |out| {
                            out.extend_from_slice(&1u32.to_be_bytes());
                            write_box(out, b"avc1", |out| {
                                out.extend_from_slice(&[0u8; 6]);
                                out.extend_from_slice(&1u16.to_be_bytes());
                                out.extend_from_slice(&[0u8; 16]);
                                out.extend_from_slice(&track.width.to_be_bytes());
                                out.extend_from_slice(&track.height.to_be_bytes());
                                out.extend_from_slice(&0x0048_0000u32.to_be_bytes());
                                out.extend_from_slice(&0x0048_0000u32.to_be_bytes());
                                out.extend_from_slice(&0u32.to_be_bytes());
                                out.extend_from_slice(&1u16.to_be_bytes());
                                out.extend_from_slice(&[0u8; 32]);
                                out.extend_from_slice(&0x0018u16.to_be_bytes());
                                out.extend_from_slice(&0xFFFFu16.to_be_bytes());
                                write_box(out, b"avcC", |out| {
                                    out.extend_from_slice(&track.avcc);
                                    Ok(())
                                })?;
                                if let Some((h, v)) = track.sar.filter(|&(h, v)| (h, v) != (1, 1)) {
                                    write_box(out, b"pasp", |out| {
                                        out.extend_from_slice(&h.to_be_bytes());
                                        out.extend_from_slice(&v.to_be_bytes());
                                        Ok(())
                                    })?;
                                }
                                Ok(())
                            })
                        })?;

                        write_full_box(out, b"stts", 0, 0, |out| {
                            out.extend_from_slice(&0u32.to_be_bytes());
                            Ok(())
                        })?;
                        write_full_box(out, b"stsc", 0, 0, |out| {
                            out.extend_from_slice(&0u32.to_be_bytes());
                            Ok(())
                        })?;
                        write_full_box(out, b"stsz", 0, 0, |out| {
                            out.extend_from_slice(&0u32.to_be_bytes());
                            out.extend_from_slice(&0u32.to_be_bytes());
                            Ok(())
                        })?;
                        write_full_box(out, b"stco", 0, 0, |out| {
                            out.extend_from_slice(&0u32.to_be_bytes());
                            Ok(())
                        })
                    })
                })
            })
        })?;

        write_box(out, b"mvex", |out| {
            write_full_box(out, b"trex", 0, 0, |out| {
                out.extend_from_slice(&COPY_TRACK_ID.to_be_bytes());
                out.extend_from_slice(&1u32.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                Ok(())
            })
        })
    })?;

    Ok(out)
}

/// Builds a `moof` + `mdat` media segment from `packets` in decode order (as demuxed).
/// `[start, end)` is the segment's plan range in ticks; `sequence` is the `mfhd` sequence number (segment index + 1).
pub fn media_segment(
    track: &CopyTrack,
    sequence: u32,
    start: i64,
    end: i64,
    packets: &[CopyPacket],
) -> Result<Vec<u8>, CopyWriteError> {
    validate_avcc(&track.avcc)?;
    if track.timescale == 0 {
        return Err(CopyWriteError::ZeroTimescale);
    }
    let Some(first) = packets.first() else {
        return Err(CopyWriteError::Empty);
    };
    if !first.keyframe {
        return Err(CopyWriteError::FirstNotKeyframe { pts: first.pts });
    }
    if first.pts != start {
        return Err(CopyWriteError::StartMismatch {
            pts: first.pts,
            start,
        });
    }
    if start < 0 {
        return Err(CopyWriteError::NegativeStart { start });
    }
    for pkt in packets {
        if pkt.pts < start {
            return Err(CopyWriteError::OpenGop {
                pts: pkt.pts,
                start,
            });
        }
    }
    for pkt in packets {
        if pkt.pts >= end {
            return Err(CopyWriteError::PastEnd { pts: pkt.pts, end });
        }
    }

    let mut dts: Vec<i64> = packets.iter().map(|p| p.pts).collect();
    dts.sort_unstable();
    for pair in dts.windows(2) {
        if pair[0] == pair[1] {
            return Err(CopyWriteError::DuplicatePts { pts: pair[0] });
        }
    }

    let sample_count = checked_u32(usize_to_i128(packets.len()), "sample count")?;
    let base_decode_time = u64::try_from(start).map_err(|_| CopyWriteError::Overflow {
        what: "tfdt base_media_decode_time",
        value: i128::from(start),
    })?;

    let payload_bytes: usize = packets.iter().map(|p| p.data.len()).sum();
    let mut out = Vec::with_capacity(128 + packets.len() * 16 + payload_bytes);
    let mut data_offset_pos = 0usize;

    write_box(&mut out, b"moof", |out| {
        write_full_box(out, b"mfhd", 0, 0, |out| {
            out.extend_from_slice(&sequence.to_be_bytes());
            Ok(())
        })?;

        write_box(out, b"traf", |out| {
            write_full_box(out, b"tfhd", 0, TFHD_DEFAULT_BASE_IS_MOOF, |out| {
                out.extend_from_slice(&COPY_TRACK_ID.to_be_bytes());
                Ok(())
            })?;

            write_full_box(out, b"tfdt", 1, 0, |out| {
                out.extend_from_slice(&base_decode_time.to_be_bytes());
                Ok(())
            })?;

            write_full_box(out, b"trun", 1, TRUN_ALL_SAMPLE_FIELDS, |out| {
                out.extend_from_slice(&sample_count.to_be_bytes());
                data_offset_pos = out.len();
                out.extend_from_slice(&0i32.to_be_bytes());

                for (i, (pkt, &d)) in packets.iter().zip(&dts).enumerate() {
                    let next_d = dts.get(i + 1).copied().unwrap_or(end);
                    let dur = checked_u32(i128::from(next_d) - i128::from(d), "sample duration")?;
                    let size = checked_u32(usize_to_i128(pkt.data.len()), "sample size")?;
                    let flags = if pkt.keyframe {
                        SAMPLE_FLAGS_SYNC
                    } else {
                        SAMPLE_FLAGS_NON_SYNC
                    };
                    let cto =
                        checked_i32(i128::from(pkt.pts) - i128::from(d), "composition offset")?;

                    out.extend_from_slice(&dur.to_be_bytes());
                    out.extend_from_slice(&size.to_be_bytes());
                    out.extend_from_slice(&flags.to_be_bytes());
                    out.extend_from_slice(&cto.to_be_bytes());
                }
                Ok(())
            })
        })
    })?;

    let moof_size = out.len();
    let data_offset = checked_i32(
        usize_to_i128(moof_size).saturating_add(8),
        "trun data_offset",
    )?;
    out[data_offset_pos..data_offset_pos + 4].copy_from_slice(&data_offset.to_be_bytes());

    write_box(&mut out, b"mdat", |out| {
        for pkt in packets {
            out.extend_from_slice(&pkt.data);
        }
        Ok(())
    })?;

    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use pharos_hls::fmp4::{
        decode_times, sample_entries, samples, timescales, track_defaults, CodecConfig,
        TrackDefaults, TrackKind,
    };

    use super::*;

    const FAKE_AVCC: [u8; 19] = [
        1, 0x64, 0x00, 0x28, 0xFF, 0xE1, 0, 4, 0x67, 0x64, 0, 0x28, 1, 0, 4, 0x68, 0xEE, 0x3C, 0x80,
    ];

    fn sample_track(timescale: u32) -> CopyTrack {
        CopyTrack {
            avcc: FAKE_AVCC.to_vec(),
            width: 1920,
            height: 1080,
            sar: None,
            timescale,
        }
    }

    #[test]
    fn codecs_string_formats_high_profile_level_40() {
        assert_eq!(codecs_string(&FAKE_AVCC).unwrap(), "avc1.640028");
        assert_eq!(
            codecs_string(&[1, 0x42, 0xC0, 0x1E, 0xFF, 0xE1, 0]).unwrap(),
            "avc1.42c01e"
        );
    }

    #[test]
    fn init_segment_round_trips_through_hls_fmp4_reader() {
        let track = sample_track(90_000);
        let init = init_segment(&track).unwrap();

        assert_eq!(timescales(&init).unwrap(), vec![(COPY_TRACK_ID, 90_000)]);
        assert_eq!(
            track_defaults(&init).unwrap(),
            vec![TrackDefaults {
                track_id: COPY_TRACK_ID,
                duration: 0,
                size: 0,
                flags: 0,
            }]
        );

        let entries = sample_entries(&init).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].track_id, COPY_TRACK_ID);
        assert_eq!(entries[0].timescale, 90_000);
        assert_eq!(&entries[0].format, b"avc1");
        assert_eq!(
            entries[0].kind,
            TrackKind::Video {
                width: 1920,
                height: 1080,
            }
        );
        assert_eq!(
            entries[0].config,
            Some(CodecConfig::Avc(FAKE_AVCC.to_vec()))
        );
    }

    #[test]
    fn init_segment_writes_pasp_only_for_non_square_sar() {
        let mut square = sample_track(1_000);
        square.sar = Some((1, 1));
        let init_sq = init_segment(&square).unwrap();
        assert!(!init_sq.windows(4).any(|w| w == b"pasp"));

        let mut anamorphic = sample_track(1_000);
        anamorphic.width = 1440;
        anamorphic.sar = Some((4, 3));
        let init_ana = init_segment(&anamorphic).unwrap();
        assert!(init_ana.windows(4).any(|w| w == b"pasp"));
        assert_eq!(sample_entries(&init_ana).unwrap().len(), 1);
    }

    #[test]
    fn media_segment_with_b_frames_round_trips_samples_and_timing() {
        let track = sample_track(1_000);
        let init = init_segment(&track).unwrap();
        let defaults = track_defaults(&init).unwrap();

        let pts_order = [0i64, 125, 42, 83, 250, 167, 208];
        let packets: Vec<CopyPacket> = pts_order
            .iter()
            .enumerate()
            .map(|(i, &pts)| CopyPacket {
                data: vec![0, 0, 0, 2, 0x65, i as u8],
                pts,
                keyframe: i == 0,
            })
            .collect();

        let start = 0i64;
        let end = 292i64;
        let seg = media_segment(&track, 1, start, end, &packets).unwrap();

        assert_eq!(decode_times(&seg).unwrap(), vec![(COPY_TRACK_ID, 0)]);

        let parsed = samples(&seg, &defaults).unwrap();
        assert_eq!(parsed.len(), packets.len());

        let expected_dts = [0u64, 42, 83, 125, 167, 208, 250];
        let got_dts: Vec<u64> = parsed.iter().map(|s| s.decode_time).collect();
        assert_eq!(got_dts, expected_dts);

        let got_pts: Vec<i64> = parsed
            .iter()
            .map(|s| s.presentation_time() as i64)
            .collect();
        assert_eq!(got_pts, pts_order);

        let total_dur: i64 = parsed.iter().map(|s| i64::from(s.duration)).sum();
        assert_eq!(total_dur, end - start);

        for (i, (sample, pkt)) in parsed.iter().zip(&packets).enumerate() {
            assert_eq!(sample.sync, i == 0);
            assert_eq!(sample.data(&seg), Some(pkt.data.as_slice()));
        }
    }

    #[test]
    fn consecutive_segments_from_plan_butt_join_tfdt_and_durations() {
        let track = sample_track(1_000);
        let init = init_segment(&track).unwrap();
        let defaults = track_defaults(&init).unwrap();

        let seg1_pkts = [
            CopyPacket {
                data: vec![1, 2, 3, 4],
                pts: 0,
                keyframe: true,
            },
            CopyPacket {
                data: vec![5, 6, 7],
                pts: 2_000,
                keyframe: false,
            },
            CopyPacket {
                data: vec![8, 9],
                pts: 4_000,
                keyframe: false,
            },
        ];
        let seg2_pkts = [
            CopyPacket {
                data: vec![10, 11, 12],
                pts: 6_000,
                keyframe: true,
            },
            CopyPacket {
                data: vec![13, 14],
                pts: 9_000,
                keyframe: false,
            },
        ];

        let seg1 = media_segment(&track, 1, 0, 6_000, &seg1_pkts).unwrap();
        let seg2 = media_segment(&track, 2, 6_000, 12_000, &seg2_pkts).unwrap();

        let tfdt1 = decode_times(&seg1).unwrap()[0].1;
        let tfdt2 = decode_times(&seg2).unwrap()[0].1;
        let dur1: u64 = samples(&seg1, &defaults)
            .unwrap()
            .iter()
            .map(|s| u64::from(s.duration))
            .sum();

        assert_eq!(tfdt1, 0);
        assert_eq!(tfdt2, tfdt1 + dur1);
    }

    #[test]
    fn error_variants_report_offending_values() {
        assert_eq!(
            codecs_string(&[]),
            Err(CopyWriteError::NotAvcc {
                first: None,
                len: 0,
            })
        );
        assert_eq!(
            codecs_string(&[1, 0x64, 0, 0x28, 0xFF, 0xE1]),
            Err(CopyWriteError::NotAvcc {
                first: Some(1),
                len: 6,
            })
        );
        assert_eq!(
            codecs_string(&[2, 0x64, 0, 0x28, 0xFF, 0xE1, 0]),
            Err(CopyWriteError::NotAvcc {
                first: Some(2),
                len: 7,
            })
        );

        let mut zero_ts = sample_track(0);
        assert_eq!(init_segment(&zero_ts), Err(CopyWriteError::ZeroTimescale));
        assert_eq!(
            media_segment(&zero_ts, 1, 0, 1_000, &[]),
            Err(CopyWriteError::ZeroTimescale)
        );
        zero_ts.avcc = vec![0; 3];
        assert_eq!(
            init_segment(&zero_ts),
            Err(CopyWriteError::NotAvcc {
                first: Some(0),
                len: 3,
            })
        );

        let track = sample_track(1_000);
        assert_eq!(
            media_segment(&track, 1, 0, 1_000, &[]),
            Err(CopyWriteError::Empty)
        );

        let non_kf = [CopyPacket {
            data: vec![1],
            pts: 0,
            keyframe: false,
        }];
        assert_eq!(
            media_segment(&track, 1, 0, 1_000, &non_kf),
            Err(CopyWriteError::FirstNotKeyframe { pts: 0 })
        );

        let mismatch = [CopyPacket {
            data: vec![1],
            pts: 500,
            keyframe: true,
        }];
        assert_eq!(
            media_segment(&track, 1, 0, 1_000, &mismatch),
            Err(CopyWriteError::StartMismatch { pts: 500, start: 0 })
        );

        let neg_start = [CopyPacket {
            data: vec![1],
            pts: -10,
            keyframe: true,
        }];
        assert_eq!(
            media_segment(&track, 1, -10, 1_000, &neg_start),
            Err(CopyWriteError::NegativeStart { start: -10 })
        );

        let open_gop = [
            CopyPacket {
                data: vec![1],
                pts: 1_000,
                keyframe: true,
            },
            CopyPacket {
                data: vec![2],
                pts: 900,
                keyframe: false,
            },
        ];
        assert_eq!(
            media_segment(&track, 1, 1_000, 2_000, &open_gop),
            Err(CopyWriteError::OpenGop {
                pts: 900,
                start: 1_000,
            })
        );

        let past_end = [
            CopyPacket {
                data: vec![1],
                pts: 1_000,
                keyframe: true,
            },
            CopyPacket {
                data: vec![2],
                pts: 2_000,
                keyframe: false,
            },
        ];
        assert_eq!(
            media_segment(&track, 1, 1_000, 2_000, &past_end),
            Err(CopyWriteError::PastEnd {
                pts: 2_000,
                end: 2_000,
            })
        );

        let dup_pts = [
            CopyPacket {
                data: vec![1],
                pts: 1_000,
                keyframe: true,
            },
            CopyPacket {
                data: vec![2],
                pts: 1_500,
                keyframe: false,
            },
            CopyPacket {
                data: vec![3],
                pts: 1_500,
                keyframe: false,
            },
        ];
        assert_eq!(
            media_segment(&track, 1, 1_000, 2_000, &dup_pts),
            Err(CopyWriteError::DuplicatePts { pts: 1_500 })
        );

        let huge_end = i64::from(u32::MAX) + 10;
        let overflow_dur = [CopyPacket {
            data: vec![1],
            pts: 0,
            keyframe: true,
        }];
        assert_eq!(
            media_segment(&track, 1, 0, huge_end, &overflow_dur),
            Err(CopyWriteError::Overflow {
                what: "sample duration",
                value: i128::from(huge_end),
            })
        );
    }
}
