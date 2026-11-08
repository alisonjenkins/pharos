//! fMP4 HLS feeder (spec 032 slice 3, player step 3): reads a media playlist,
//! its init segment and its segments from a directory, and hands the first
//! video track's samples to `AMediaCodec` as Annex B, parsed by `pharos-hls`
//! (spec 032 decision 4) instead of `AMediaExtractor`.
//!
//! `setprop debug.pharos.video <dir>` with an `index.m3u8` in `<dir>` picks
//! this source. Network fetches come later; the planner already plans them.

use pharos_hls::annexb::{self, ParameterSets};
use pharos_hls::fmp4::{self, CodecConfig, Sample, TrackKind};
use pharos_hls::playlist;
use std::path::{Path, PathBuf};

/// The playlist a feeder directory must hold.
pub const PLAYLIST: &str = "index.m3u8";

/// Headroom over the largest sample for the codec's input buffers.
const INPUT_HEADROOM: usize = 64 * 1024;

pub enum Codec {
    Avc,
    Hevc,
}

impl Codec {
    pub fn mime(&self) -> &'static str {
        match self {
            Self::Avc => "video/avc",
            Self::Hevc => "video/hevc",
        }
    }
}

/// What `AMediaFormat` needs to configure the decoder.
pub struct Format {
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    pub csd: Vec<Vec<u8>>,
    pub max_input: usize,
}

pub enum Next {
    /// `len` bytes of Annex B were written; `pts_us` and whether it is a key frame.
    Sample { len: usize, pts_us: u64, key: bool },
    /// The sample does not fit the codec's buffer.
    TooBig { need: usize },
    End,
}

pub struct Feeder {
    format: Format,
    timescale: u32,
    length_size: u8,
    segments: Vec<(Vec<u8>, Vec<Sample>)>,
    segment: usize,
    sample: usize,
    scratch: Vec<u8>,
}

fn resolve(dir: &Path, uri: &str) -> PathBuf {
    dir.join(uri.trim_start_matches("file://").rsplit('/').next().unwrap_or(uri))
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

impl Feeder {
    pub fn is_feeder_dir(path: &str) -> bool {
        Path::new(path).join(PLAYLIST).is_file()
    }

    pub fn open(dir: &str) -> Result<Self, String> {
        let dir = Path::new(dir);
        let text = String::from_utf8(read(&dir.join(PLAYLIST))?)
            .map_err(|e| format!("{PLAYLIST}: not UTF-8: {e}"))?;
        let base = format!("file://{}/", dir.display());
        let media = playlist::parse_media(&text, &base).map_err(|e| format!("{PLAYLIST}: {e}"))?;
        let map = media.init_uri.as_deref().ok_or("playlist has no EXT-X-MAP")?;
        let init = read(&resolve(dir, map))?;
        let defaults = fmp4::track_defaults(&init).map_err(|e| format!("init: {e}"))?;
        let entries = fmp4::sample_entries(&init).map_err(|e| format!("init: {e}"))?;
        let entry = entries
            .iter()
            .find(|e| matches!(e.kind, TrackKind::Video { .. }))
            .ok_or("init has no video track")?;
        let TrackKind::Video { width, height } = entry.kind else {
            return Err("init has no video track".into());
        };
        let (codec, sets): (Codec, ParameterSets) = match &entry.config {
            Some(CodecConfig::Avc(avcc)) => (
                Codec::Avc,
                annexb::avc_parameter_sets(avcc).map_err(|e| format!("avcC: {e}"))?,
            ),
            Some(CodecConfig::Hevc(hvcc)) => (
                Codec::Hevc,
                annexb::hevc_parameter_sets(hvcc).map_err(|e| format!("hvcC: {e}"))?,
            ),
            other => {
                return Err(format!(
                    "video codec {} ({other:?}) not fed yet",
                    String::from_utf8_lossy(&entry.format)
                ))
            }
        };
        let csd = match codec {
            Codec::Avc => {
                let (sps, pps) = sets.avc_csd();
                vec![sps, pps]
            }
            Codec::Hevc => vec![sets.hevc_csd()],
        };

        let mut segments = Vec::with_capacity(media.segments.len());
        let mut largest = 0usize;
        for seg in &media.segments {
            let bytes = read(&resolve(dir, &seg.uri))?;
            let samples: Vec<Sample> = fmp4::samples(&bytes, &defaults)
                .map_err(|e| format!("{}: {e}", seg.uri))?
                .into_iter()
                .filter(|s| s.track_id == entry.track_id)
                .collect();
            largest = samples.iter().map(|s| s.size as usize).fold(largest, usize::max);
            segments.push((bytes, samples));
        }
        let count: usize = segments.iter().map(|(_, s)| s.len()).sum();
        log::info!(
            "video: source=fmp4 codec={} {width}x{height} track={} segments={} samples={count} largest={largest}",
            codec.mime(),
            entry.track_id,
            segments.len(),
        );
        Ok(Self {
            format: Format { codec, width, height, csd, max_input: largest + INPUT_HEADROOM },
            timescale: entry.timescale,
            length_size: sets.length_size,
            segments,
            segment: 0,
            sample: 0,
            scratch: Vec::new(),
        })
    }

    pub fn format(&self) -> &Format {
        &self.format
    }

    pub fn rewind(&mut self) {
        self.segment = 0;
        self.sample = 0;
    }

    /// Writes the next sample into `buf` as Annex B.
    pub fn next(&mut self, buf: &mut [u8]) -> Result<Next, String> {
        loop {
            let Some((bytes, samples)) = self.segments.get(self.segment) else {
                return Ok(Next::End);
            };
            let Some(sample) = samples.get(self.sample) else {
                self.segment += 1;
                self.sample = 0;
                continue;
            };
            let data = sample.data(bytes).ok_or("sample outside its segment")?;
            self.scratch.clear();
            annexb::to_annex_b(data, self.length_size, &mut self.scratch)
                .map_err(|e| format!("segment {} sample {}: {e}", self.segment, self.sample))?;
            let len = self.scratch.len();
            let Some(dst) = buf.get_mut(..len) else {
                return Ok(Next::TooBig { need: len });
            };
            dst.copy_from_slice(&self.scratch);
            self.sample += 1;
            let pts_us = fmp4::to_ms(sample.presentation_time().saturating_mul(1_000), self.timescale);
            return Ok(Next::Sample { len, pts_us, key: sample.sync });
        }
    }
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected results is the intended failure mode in tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{Codec, Feeder, Next, PLAYLIST};

    /// The ffmpeg H.264 fixture of `pharos-hls`, laid out as a feeder directory.
    fn fixture_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pharos-feeder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = concat!(env!("CARGO_MANIFEST_DIR"), "/../pharos-hls/testdata");
        for f in ["init.mp4", "seg0.m4s"] {
            std::fs::copy(format!("{data}/{f}"), dir.join(f)).unwrap();
        }
        std::fs::write(
            dir.join(PLAYLIST),
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n#EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1.0,\nseg0.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn feeds_every_video_sample_as_annex_b_then_ends() {
        let dir = fixture_dir();
        let mut feeder = Feeder::open(dir.to_str().unwrap()).unwrap();
        let f = feeder.format();
        assert!(matches!(f.codec, Codec::Avc));
        assert_eq!((f.width, f.height, f.csd.len()), (64, 36, 2));
        assert_eq!(&f.csd[0][..5], &[0, 0, 0, 1, 0x67]);
        assert_eq!(&f.csd[1][..5], &[0, 0, 0, 1, 0x68]);

        let mut buf = vec![0u8; f.max_input];
        let mut fed = Vec::new();
        while let Next::Sample { len, pts_us, key } = feeder.next(&mut buf).unwrap() {
            assert_eq!(&buf[..4], &[0, 0, 0, 1]);
            fed.push((len, pts_us, key));
        }
        assert_eq!(fed.len(), 30);
        assert_eq!(fed.iter().filter(|s| s.2).count(), 1);
        assert!(fed[0].2);
        // The first frame shows at 1024 / 15360 s, after its composition offset.
        assert_eq!(fed[0].1, 66_666);

        feeder.rewind();
        let mut small = [0u8; 8];
        assert!(matches!(feeder.next(&mut small).unwrap(), Next::TooBig { .. }));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
