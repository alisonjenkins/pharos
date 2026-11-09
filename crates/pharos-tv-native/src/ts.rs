//! MPEG-TS video demux (spec 032 slice 3): the server sends native clients
//! muxed MPEG-TS, not fMP4 (`is_web_client` in the server's `hls.rs` keeps
//! fMP4 for browsers), and its `main` rung carries the title at source
//! resolution. This pulls the video access units and the AAC frames out of
//! one segment, for the video feeder and the audio player.
//!
//! H.264 and HEVC in TS are already Annex B with their parameter sets in
//! band, so an access unit goes to the decoder as it is.

/// TS packet size.
const PACKET: usize = 188;
const SYNC: u8 = 0x47;
const PAT_PID: u16 = 0;

/// ISO 13818-1 `stream_type` values the feeder decodes.
pub const STREAM_AVC: u8 = 0x1b;
pub const STREAM_HEVC: u8 = 0x24;
/// AAC in ADTS.
pub const STREAM_AAC: u8 = 0x0f;

/// One coded picture, with its presentation time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub pts_us: u64,
    pub key: bool,
    pub data: Vec<u8>,
}

/// One segment's elementary streams.
#[derive(Debug, Default)]
pub struct Demuxed {
    pub video: Vec<AccessUnit>,
    /// AAC frames, one ADTS frame each, header included; `key` is always true.
    pub audio: Vec<AccessUnit>,
}

/// The PIDs found so far; kept across segments, since a segment that misses
/// its PAT or PMT still carries the same streams.
#[derive(Debug, Default)]
pub struct Demux {
    pmt_pid: Option<u16>,
    video: Option<(u16, u8)>,
    audio: Option<u16>,
}

impl Demux {
    /// The video stream's `stream_type`, once a PMT has named it.
    pub fn stream_type(&self) -> Option<u8> {
        self.video.map(|(_, t)| t)
    }

    /// Every video access unit in `segment`, in decode order.
    pub fn access_units(&mut self, segment: &[u8]) -> Result<Vec<AccessUnit>, String> {
        Ok(self.demux(segment)?.video)
    }

    /// Every video access unit and AAC frame in `segment`, in decode order.
    /// A PES still open at the segment's end is complete: the server's
    /// segmenter cuts on access units.
    pub fn demux(&mut self, segment: &[u8]) -> Result<Demuxed, String> {
        let mut out = Demuxed::default();
        let mut video: Option<Vec<u8>> = None;
        let mut audio: Option<Vec<u8>> = None;
        for (n, pkt) in segment.chunks(PACKET).enumerate() {
            if pkt.len() < PACKET {
                break;
            }
            if pkt[0] != SYNC {
                return Err(format!("packet {n}: lost sync"));
            }
            let start = pkt[1] & 0x40 != 0;
            let pid = (u16::from(pkt[1] & 0x1f) << 8) | u16::from(pkt[2]);
            let control = (pkt[3] >> 4) & 3;
            if control & 1 == 0 {
                continue;
            }
            let mut at = 4;
            if control & 2 != 0 {
                at += 1 + usize::from(pkt[4]);
            }
            let Some(payload) = pkt.get(at..) else {
                return Err(format!("packet {n}: adaptation field overruns"));
            };
            if pid == PAT_PID && start {
                self.pmt_pid = pat(payload).or(self.pmt_pid);
            } else if Some(pid) == self.pmt_pid && start {
                if let Some((v, a)) = pmt(payload) {
                    self.video = v.or(self.video);
                    self.audio = a.or(self.audio);
                }
            } else if Some(pid) == self.video.map(|(p, _)| p) {
                if start {
                    if let Some(done) = video.replace(payload.to_vec()) {
                        out.video.extend(self.unit(&done)?);
                    }
                } else if let Some(buf) = video.as_mut() {
                    buf.extend_from_slice(payload);
                }
            } else if Some(pid) == self.audio {
                if start {
                    if let Some(done) = audio.replace(payload.to_vec()) {
                        adts_frames(&done, &mut out.audio)?;
                    }
                } else if let Some(buf) = audio.as_mut() {
                    buf.extend_from_slice(payload);
                }
            }
        }
        if let Some(done) = video {
            out.video.extend(self.unit(&done)?);
        }
        if let Some(done) = audio {
            adts_frames(&done, &mut out.audio)?;
        }
        Ok(out)
    }

    fn unit(&self, pes: &[u8]) -> Result<Option<AccessUnit>, String> {
        let Some((pts_us, data)) = pes_payload(pes)? else {
            return Ok(None);
        };
        let hevc = self.stream_type() == Some(STREAM_HEVC);
        let key = nal_types(data, hevc).any(|t| is_key(t, hevc));
        Ok(Some(AccessUnit {
            pts_us,
            key,
            data: data.to_vec(),
        }))
    }
}

/// A PES packet's presentation time in µs and its payload; `None` without a
/// PTS, which leaves nothing a decoder can schedule.
fn pes_payload(pes: &[u8]) -> Result<Option<(u64, &[u8])>, String> {
    if pes.len() < 9 || pes[..3] != [0, 0, 1] {
        return Err("PES without a start code".into());
    }
    let header = 9 + usize::from(pes[8]);
    if pes[7] & 0x80 == 0 {
        return Ok(None);
    }
    let pts = timestamp(pes.get(9..14).ok_or("PES header cut short")?);
    let data = pes.get(header..).ok_or("PES header overruns")?;
    // 90 kHz to µs.
    Ok(Some((pts * 100 / 9, data)))
}

/// ADTS `sampling_frequency_index` to Hz.
const ADTS_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// What an ADTS header says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsConfig {
    /// MPEG-4 audio object type (2 for AAC LC).
    pub object_type: u8,
    pub rate_index: u8,
    pub sample_rate: u32,
    pub channels: u8,
}

impl AdtsConfig {
    pub fn parse(frame: &[u8]) -> Option<Self> {
        if frame.len() < 7 || frame[0] != 0xff || frame[1] & 0xf0 != 0xf0 {
            return None;
        }
        let rate_index = (frame[2] >> 2) & 0x0f;
        Some(Self {
            object_type: (frame[2] >> 6) + 1,
            rate_index,
            sample_rate: *ADTS_RATES.get(usize::from(rate_index))?,
            channels: ((frame[2] & 1) << 2) | (frame[3] >> 6),
        })
    }

    /// The two-byte `AudioSpecificConfig` a decoder takes as csd-0.
    pub fn audio_specific_config(&self) -> [u8; 2] {
        let v = (u16::from(self.object_type) << 11)
            | (u16::from(self.rate_index) << 7)
            | (u16::from(self.channels) << 3);
        v.to_be_bytes()
    }
}

/// The raw AAC payload of an ADTS frame, after its 7- or 9-byte header.
pub fn adts_payload(frame: &[u8]) -> &[u8] {
    let header = if frame.get(1).is_some_and(|b| b & 1 == 0) { 9 } else { 7 };
    frame.get(header..).unwrap_or_default()
}

/// Splits an AAC PES into its ADTS frames, each timed from the PES's PTS at
/// 1024 samples a raw data block.
fn adts_frames(pes: &[u8], out: &mut Vec<AccessUnit>) -> Result<(), String> {
    let Some((pts_us, mut data)) = pes_payload(pes)? else {
        return Ok(());
    };
    let mut samples = 0u64;
    while data.len() >= 7 {
        let config = AdtsConfig::parse(data).ok_or("audio PES: lost ADTS sync")?;
        let len = (usize::from(data[3] & 3) << 11) | (usize::from(data[4]) << 3) | usize::from(data[5] >> 5);
        let frame = data.get(..len).filter(|_| len >= 7).ok_or("audio PES: ADTS frame cut short")?;
        out.push(AccessUnit {
            pts_us: pts_us + samples * 1_000_000 / u64::from(config.sample_rate),
            key: true,
            data: frame.to_vec(),
        });
        samples += 1024 * (u64::from(data[6] & 3) + 1);
        data = &data[len..];
    }
    Ok(())
}

/// The first program's PMT PID.
fn pat(payload: &[u8]) -> Option<u16> {
    let table = section(payload)?;
    let end = section_end(table)?;
    table.get(8..end)?.as_chunks::<4>().0.iter().find_map(|p| {
        let program = u16::from_be_bytes([p[0], p[1]]);
        (program != 0).then(|| (u16::from(p[2] & 0x1f) << 8) | u16::from(p[3]))
    })
}

/// The first H.264 or HEVC stream's PID and type, and the first AAC (ADTS)
/// stream's PID.
fn pmt(payload: &[u8]) -> Option<(Option<(u16, u8)>, Option<u16>)> {
    let table = section(payload)?;
    let end = section_end(table)?;
    let info = (usize::from(table.get(10)? & 0x0f) << 8) | usize::from(*table.get(11)?);
    let mut at = 12 + info;
    let (mut video, mut audio) = (None, None);
    while at + 5 <= end {
        let kind = table[at];
        let pid = (u16::from(table[at + 1] & 0x1f) << 8) | u16::from(table[at + 2]);
        let es_info = (usize::from(table[at + 3] & 0x0f) << 8) | usize::from(table[at + 4]);
        if (kind == STREAM_AVC || kind == STREAM_HEVC) && video.is_none() {
            video = Some((pid, kind));
        } else if kind == STREAM_AAC && audio.is_none() {
            audio = Some(pid);
        }
        at += 5 + es_info;
    }
    Some((video, audio))
}

/// The section after the pointer field.
fn section(payload: &[u8]) -> Option<&[u8]> {
    payload.get(1 + usize::from(*payload.first()?)..)
}

/// Where the section's entries end: before its CRC.
fn section_end(table: &[u8]) -> Option<usize> {
    let length = (usize::from(table.get(1)? & 0x0f) << 8) | usize::from(*table.get(2)?);
    let end = (3 + length).checked_sub(4)?;
    (end <= table.len()).then_some(end)
}

/// A 33-bit PES timestamp.
fn timestamp(b: &[u8]) -> u64 {
    (u64::from((b[0] >> 1) & 7) << 30)
        | (u64::from(b[1]) << 22)
        | (u64::from(b[2] >> 1) << 15)
        | (u64::from(b[3]) << 7)
        | u64::from(b[4] >> 1)
}

/// The NAL units of an Annex B buffer, start codes included.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            // Keep a four-byte start code's leading zero with its NAL.
            starts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts.iter().skip(1).copied().chain([data.len()]).collect();
    starts.into_iter().zip(ends).map(move |(s, e)| &data[s..e])
}

/// A NAL's type, after its start code.
fn nal_type(nal: &[u8], hevc: bool) -> Option<u8> {
    let at = nal.iter().position(|&b| b == 1)? + 1;
    let head = *nal.get(at)?;
    Some(if hevc { (head >> 1) & 0x3f } else { head & 0x1f })
}

fn nal_types(data: &[u8], hevc: bool) -> impl Iterator<Item = u8> + '_ {
    nal_units(data).filter_map(move |n| nal_type(n, hevc))
}

/// IDR for H.264; IRAP (BLA, IDR, CRA) for HEVC.
fn is_key(nal_type: u8, hevc: bool) -> bool {
    if hevc {
        (16..=21).contains(&nal_type)
    } else {
        nal_type == 5
    }
}

/// The decoder's codec-specific data from a key frame's in-band parameter
/// sets: SPS then PPS for H.264 (csd-0, csd-1); VPS, SPS and PPS together
/// for HEVC (csd-0).
pub fn csd(key_frame: &[u8], hevc: bool) -> Vec<Vec<u8>> {
    let pick = |types: &[u8]| -> Vec<u8> {
        nal_units(key_frame)
            .filter(|n| nal_type(n, hevc).is_some_and(|t| types.contains(&t)))
            .flat_map(|n| n.iter().copied())
            .collect()
    };
    if hevc {
        vec![pick(&[32, 33, 34])]
    } else {
        vec![pick(&[7]), pick(&[8])]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const PMT_PID: u16 = 0x1000;
    const VIDEO_PID: u16 = 0x100;
    const AUDIO_PID: u16 = 0x101;

    /// A TS packet carrying `payload` (stuffed with an adaptation field).
    fn packet(pid: u16, start: bool, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184);
        let mut p = vec![SYNC, (u8::from(start) << 6) | (pid >> 8) as u8, pid as u8];
        let stuffing = 184 - payload.len();
        if stuffing == 0 {
            p.push(0x10);
        } else {
            p.push(0x30);
            p.push((stuffing - 1) as u8);
            if stuffing > 1 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xff, stuffing - 2));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), PACKET);
        p
    }

    /// A PSI section with pointer field and a dummy CRC.
    fn psi(table_id: u8, body: &[u8]) -> Vec<u8> {
        let length = body.len() + 5 + 4;
        let mut s = vec![0, table_id, 0xb0 | (length >> 8) as u8, length as u8];
        s.extend_from_slice(&[0, 1, 0xc1, 0, 0]);
        s.extend_from_slice(body);
        s.extend_from_slice(&[0; 4]);
        s
    }

    fn pat_packet() -> Vec<u8> {
        packet(PAT_PID, true, &psi(0, &[0, 1, 0xe0 | (PMT_PID >> 8) as u8, PMT_PID as u8]))
    }

    fn pmt_packet(video_type: u8) -> Vec<u8> {
        let mut body = vec![0xe1, 0x00, 0xf0, 0x00];
        body.extend_from_slice(&[0x0f, 0xe0 | (AUDIO_PID >> 8) as u8, AUDIO_PID as u8, 0xf0, 0]);
        body.extend_from_slice(&[video_type, 0xe0 | (VIDEO_PID >> 8) as u8, VIDEO_PID as u8, 0xf0, 0]);
        packet(PMT_PID, true, &psi(2, &body))
    }

    fn pts_bytes(pts: u64, marker: u8) -> [u8; 5] {
        [
            (marker << 4) | (((pts >> 30) as u8 & 7) << 1) | 1,
            (pts >> 22) as u8,
            (((pts >> 15) as u8) << 1) | 1,
            (pts >> 7) as u8,
            ((pts as u8) << 1) | 1,
        ]
    }

    /// One access unit as PES packets, split across TS packets.
    fn pes_packets(pid: u16, pts: u64, es: &[u8]) -> Vec<u8> {
        let mut pes = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5];
        pes.extend_from_slice(&pts_bytes(pts, 2));
        pes.extend_from_slice(es);
        let mut out = Vec::new();
        for (i, chunk) in pes.chunks(184).enumerate() {
            out.extend(packet(pid, i == 0, chunk));
        }
        out
    }

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x64, 0, 0x28];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xee, 0x3c];

    fn idr() -> Vec<u8> {
        let mut au = [SPS, PPS].concat();
        au.extend_from_slice(&[0, 0, 1, 0x65]);
        au.extend(std::iter::repeat_n(0xab, 400));
        au
    }

    fn segment(video_type: u8) -> Vec<u8> {
        let mut seg = pat_packet();
        seg.extend(pmt_packet(video_type));
        seg.extend(pes_packets(VIDEO_PID, 126_000, &idr()));
        seg.extend(pes_packets(AUDIO_PID, 126_000, &[0xff, 0xf1, 1, 2]));
        seg.extend(pes_packets(VIDEO_PID, 129_750, &[0, 0, 0, 1, 0x41, 1, 2, 3]));
        seg
    }

    #[test]
    fn pulls_each_video_access_unit_with_its_time() {
        let mut demux = Demux::default();
        let units = demux.access_units(&segment(STREAM_AVC)).unwrap();
        assert_eq!(demux.stream_type(), Some(STREAM_AVC));
        assert_eq!(units.len(), 2, "two video access units");
        assert_eq!(units[0].data, idr(), "a PES split over three packets is whole");
        assert!(units[0].key);
        assert_eq!(units[0].pts_us, 1_400_000, "126000 at 90 kHz");
        assert!(!units[1].key);
        assert_eq!(units[1].pts_us, 1_441_666);
    }

    /// An AAC LC, 48 kHz, stereo ADTS frame of `len` bytes.
    fn adts(len: usize) -> Vec<u8> {
        let mut f = vec![
            0xff,
            0xf1,
            0x4c,
            0x80 | ((len >> 11) & 3) as u8,
            (len >> 3) as u8,
            (((len & 7) << 5) | 0x1f) as u8,
            0xfc,
        ];
        f.resize(len, 0x21);
        f
    }

    #[test]
    fn splits_an_audio_pes_into_timed_adts_frames() {
        let mut seg = pat_packet();
        seg.extend(pmt_packet(STREAM_AVC));
        let pes = [adts(150), adts(220)].concat();
        seg.extend(pes_packets(AUDIO_PID, 90_000, &pes));
        let out = Demux::default().demux(&seg).unwrap();
        assert_eq!(out.audio.len(), 2);
        assert_eq!(out.audio[0].data, adts(150));
        assert_eq!(out.audio[0].pts_us, 1_000_000);
        assert_eq!(out.audio[1].pts_us, 1_021_333, "1024 samples at 48 kHz later");
        let config = AdtsConfig::parse(&out.audio[0].data).unwrap();
        assert_eq!((config.object_type, config.sample_rate, config.channels), (2, 48_000, 2));
        assert_eq!(config.audio_specific_config(), [0x11, 0x90]);
        assert_eq!(adts_payload(&out.audio[0].data).len(), 143);
    }

    #[test]
    fn a_segment_without_tables_keeps_the_pids_already_found() {
        let mut demux = Demux::default();
        demux.access_units(&segment(STREAM_AVC)).unwrap();
        let bare = pes_packets(VIDEO_PID, 200_000, &idr());
        assert_eq!(demux.access_units(&bare).unwrap().len(), 1);
    }

    #[test]
    fn csd_is_the_key_frames_parameter_sets() {
        assert_eq!(csd(&idr(), false), [SPS.to_vec(), PPS.to_vec()]);
        let hevc = [
            &[0, 0, 0, 1, 0x40, 1, 9][..],
            &[0, 0, 0, 1, 0x42, 1, 8],
            &[0, 0, 0, 1, 0x44, 1, 7],
            &[0, 0, 1, 0x26, 1, 0xaa],
        ]
        .concat();
        assert_eq!(csd(&hevc, true), [hevc[..21].to_vec()]);
        let mut demux = Demux::default();
        let mut seg = pat_packet();
        seg.extend(pmt_packet(STREAM_HEVC));
        seg.extend(pes_packets(VIDEO_PID, 9_000, &hevc));
        let units = demux.access_units(&seg).unwrap();
        assert!(units[0].key, "IDR_W_RADL (19) is a key frame");
    }

    #[test]
    fn rejects_a_stream_that_lost_sync() {
        let mut seg = segment(STREAM_AVC);
        seg[PACKET] = 0;
        assert!(Demux::default().access_units(&seg).is_err());
    }

    /// A real segment from the server: `PHAROS_TS_SEGMENT=<file.ts>`.
    #[test]
    #[ignore = "needs a server segment on disk"]
    fn reads_a_server_segment() {
        let path = std::env::var("PHAROS_TS_SEGMENT").unwrap();
        let mut demux = Demux::default();
        let bytes = std::fs::read(path).unwrap();
        let started = std::time::Instant::now();
        let Demuxed { video: units, audio } = demux.demux(&bytes).unwrap();
        println!(
            "audio frames={} first_pts_us={:?} config={:?}",
            audio.len(),
            audio.first().map(|a| a.pts_us),
            audio.first().and_then(|a| AdtsConfig::parse(&a.data))
        );
        println!("demux bytes={} ms={:.2}", bytes.len(), started.elapsed().as_secs_f64() * 1e3);
        let keys = units.iter().filter(|u| u.key).count();
        println!(
            "type={:?} units={} keys={keys} first_pts_us={} csd={:?}",
            demux.stream_type(),
            units.len(),
            units[0].pts_us,
            csd(&units[0].data, demux.stream_type() == Some(STREAM_HEVC))
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>()
        );
        assert!(units[0].key, "a segment opens on a key frame");
    }
}
