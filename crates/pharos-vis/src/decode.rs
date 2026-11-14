//! Opens a song and hands out its samples as mono `f32`, one packet at a
//! time, with seeking. Only the codecs the library really holds are built.

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

/// Packets that fail to decode in a row before the song is given up on.
const MAX_BAD_PACKETS: u32 = 50;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The container or codec is not one that is built in (Opus, for one).
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Failed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("{0} packets in a row would not decode")]
    Corrupt(u32),
    #[error("{0}")]
    Failed(String),
}

/// What one read gave.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// This many mono samples were appended.
    Samples(usize),
    End,
}

pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    rate: u32,
    codec: &'static str,
    /// The mono sample index of the next sample read.
    position: u64,
    interleaved: Vec<f32>,
}

fn open_error(e: SymError) -> OpenError {
    match e {
        SymError::Unsupported(what) => OpenError::Unsupported(what.to_owned()),
        other => OpenError::Failed(other.to_string()),
    }
}

impl Decoder {
    /// `extension` (such as `"flac"`) only helps the format guess.
    pub fn open(source: Box<dyn MediaSource>, extension: Option<&str>) -> Result<Self, OpenError> {
        let mss = MediaSourceStream::new(source, Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = extension {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(open_error)?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| OpenError::Unsupported("no audio track".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| OpenError::Unsupported("no audio codec parameters".into()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(open_error)?;
        let rate = params
            .sample_rate
            .ok_or_else(|| OpenError::Failed("the sample rate is unknown".into()))?;
        let (track_id, time_base) = (track.id, track.time_base);
        let codec = decoder.codec_info().short_name;
        Ok(Self {
            format,
            decoder,
            track_id,
            time_base,
            rate,
            codec,
            position: 0,
            interleaved: Vec::new(),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub fn codec(&self) -> &'static str {
        self.codec
    }

    /// The mono sample index of the next sample [`Decoder::read`] appends.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Decode the next packet and append it to `out` as mono (the channels
    /// averaged). A packet that will not decode is skipped; a run of them
    /// ends the song with an error.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<Step, DecodeError> {
        let mut bad = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(Step::End),
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(Step::End)
                }
                Err(e) => return Err(DecodeError::Failed(e.to_string())),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let n = mix_to_mono(&buf, &mut self.interleaved, out);
                    self.position += n as u64;
                    return Ok(Step::Samples(n));
                }
                Err(SymError::DecodeError(_)) => {
                    bad += 1;
                    if bad >= MAX_BAD_PACKETS {
                        return Err(DecodeError::Corrupt(bad));
                    }
                }
                Err(e) => return Err(DecodeError::Failed(e.to_string())),
            }
        }
    }

    /// Move to `ms` into the song (at or just before it, as the container
    /// allows) and return where it landed, in milliseconds.
    pub fn seek(&mut self, ms: u64) -> Result<u64, DecodeError> {
        let to = SeekTo::Time {
            time: Time::from_millis_u64(ms),
            track_id: Some(self.track_id),
        };
        let seeked = self
            .format
            .seek(SeekMode::Accurate, to)
            .map_err(|e| DecodeError::Failed(format!("seek to {ms} ms: {e}")))?;
        self.decoder.reset();
        let landed_ms = self
            .time_base
            .and_then(|tb| tb.calc_time(seeked.actual_ts))
            .map(|t| t.as_millis().max(0) as u64)
            .unwrap_or(ms);
        self.position = landed_ms.saturating_mul(u64::from(self.rate)) / 1000;
        Ok(landed_ms)
    }
}

/// Append `buf` to `out` as mono; returns the frames added.
fn mix_to_mono(
    buf: &GenericAudioBufferRef<'_>,
    scratch: &mut Vec<f32>,
    out: &mut Vec<f32>,
) -> usize {
    let channels = buf.num_planes().max(1);
    let frames = buf.frames();
    scratch.clear();
    buf.copy_to_vec_interleaved(scratch);
    if channels == 1 {
        out.extend_from_slice(scratch);
    } else {
        let scale = 1.0 / channels as f32;
        out.extend(
            scratch
                .chunks(channels)
                .map(|c| c.iter().sum::<f32>() * scale),
        );
    }
    frames
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::fixture;

    fn open(bytes: Vec<u8>, ext: &str) -> Result<Decoder, OpenError> {
        Decoder::open(Box::new(Cursor::new(bytes)), Some(ext))
    }

    fn read_all(d: &mut Decoder) -> Vec<f32> {
        let mut out = Vec::new();
        while let Ok(Step::Samples(_)) = d.read(&mut out) {}
        out
    }

    #[test]
    fn a_wav_decodes_to_the_samples_that_made_it() {
        let src = fixture::sine(440.0, 8000, 0.5, 0.5);
        let mut d = open(fixture::wav(&src, 8000, 1), "wav").unwrap();
        assert_eq!(d.sample_rate(), 8000);
        let got = read_all(&mut d);
        assert_eq!(got.len(), src.len());
        for (g, s) in got.iter().zip(&src) {
            assert!((g - f32::from(*s) / 32768.0).abs() < 1e-3);
        }
        assert_eq!(d.position(), src.len() as u64);
    }

    #[test]
    fn a_flac_decodes_to_the_samples_that_made_it() {
        let src = fixture::sine(440.0, 44100, 0.3, 0.5);
        let mut d = open(fixture::flac(&src, 44100), "flac").unwrap();
        assert_eq!(d.sample_rate(), 44100);
        assert_eq!(d.codec(), "flac");
        let got = read_all(&mut d);
        assert_eq!(got.len(), src.len());
        for (g, s) in got.iter().zip(&src) {
            assert!((g - f32::from(*s) / 32768.0).abs() < 1e-3);
        }
    }

    #[test]
    fn stereo_is_averaged_to_mono() {
        let inter: Vec<i16> = (0..400).flat_map(|_| [16384, -16384 + 8192]).collect();
        let mut d = open(fixture::wav(&inter, 8000, 2), "wav").unwrap();
        let got = read_all(&mut d);
        assert_eq!(got.len(), 400);
        assert!((got[0] - (0.5 - 0.25) / 2.0).abs() < 1e-3, "{}", got[0]);
    }

    #[test]
    fn a_seek_lands_at_or_before_and_the_position_follows() {
        let src = fixture::sine(440.0, 8000, 2.0, 0.5);
        let mut d = open(fixture::wav(&src, 8000, 1), "wav").unwrap();
        let landed = d.seek(1500).unwrap();
        assert!((1400..=1500).contains(&landed), "landed {landed}");
        assert_eq!(d.position(), landed * 8);
        let got = read_all(&mut d);
        assert_eq!(got.len() as u64, src.len() as u64 - landed * 8);
    }

    #[test]
    fn bytes_that_are_no_audio_are_unsupported_not_a_crash() {
        let e = open(vec![7u8; 4096], "opus").err().unwrap();
        assert!(matches!(e, OpenError::Unsupported(_)), "{e}");
    }

    #[test]
    fn a_cut_off_file_ends_instead_of_failing() {
        let src = fixture::sine(440.0, 8000, 1.0, 0.5);
        let mut bytes = fixture::wav(&src, 8000, 1);
        bytes.truncate(bytes.len() - 1001);
        let mut d = open(bytes, "wav").unwrap();
        assert!(read_all(&mut d).len() < src.len());
    }
}
