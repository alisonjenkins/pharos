//! One visualiser frame and its compact wire form.

/// Frames per second the visualiser is fed.
pub const FPS: u64 = 30;
/// Samples the analyser looks at per frame (the web's `fftSize`).
pub const FFT_SIZE: usize = 2048;
/// Frequency bins per frame.
pub const BINS: usize = FFT_SIZE / 2;
/// Decibels per step of a quantised bin.
pub const DB_STEP: f32 = 0.5;
/// A bin with no sound (or quieter than the scale reaches).
pub const SILENT_BIN: u8 = u8::MAX;

/// A frame as stored and sent: bins and waveform quantised to a byte each,
/// so a 2 s ring is about 180 KB instead of 720 KB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// `BINS` levels; byte `b` is `-b * DB_STEP` dBFS, `SILENT_BIN` none.
    pub bins: Vec<u8>,
    /// `FFT_SIZE` samples; `s` is `s / 127`, from -1 to 1.
    pub wave: Vec<i8>,
}

/// A bin's level in dBFS to its byte.
pub fn quantise_db(db: f32) -> u8 {
    if !db.is_finite() {
        return SILENT_BIN;
    }
    let steps = (-db / DB_STEP).round();
    if steps >= f32::from(SILENT_BIN) {
        SILENT_BIN
    } else {
        steps.max(0.0) as u8
    }
}

/// A bin's byte back to dBFS (`-inf` for none).
pub fn dequantise_db(byte: u8) -> f32 {
    if byte == SILENT_BIN {
        f32::NEG_INFINITY
    } else {
        -f32::from(byte) * DB_STEP
    }
}

pub fn quantise_sample(s: f32) -> i8 {
    (s.clamp(-1.0, 1.0) * 127.0).round() as i8
}

pub fn dequantise_sample(s: i8) -> f32 {
    f32::from(s) / 127.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_survives_the_byte_within_a_step() {
        for db in [0.0, -3.1, -13.56, -60.0, -126.0] {
            let back = dequantise_db(quantise_db(db));
            assert!((back - db).abs() <= DB_STEP / 2.0 + 1e-4, "{db} -> {back}");
        }
    }

    #[test]
    fn silence_and_nan_are_the_silent_byte_and_loud_clips_to_zero_db() {
        assert_eq!(quantise_db(f32::NEG_INFINITY), SILENT_BIN);
        assert_eq!(quantise_db(f32::NAN), SILENT_BIN);
        assert_eq!(quantise_db(-500.0), SILENT_BIN);
        assert_eq!(quantise_db(6.0), 0);
        assert_eq!(dequantise_db(SILENT_BIN), f32::NEG_INFINITY);
    }

    #[test]
    fn samples_round_trip_and_clip() {
        assert_eq!(quantise_sample(1.7), 127);
        assert_eq!(quantise_sample(-1.7), -127);
        assert!((dequantise_sample(quantise_sample(0.5)) - 0.5).abs() < 0.005);
    }
}

/// The page decodes frames with constants of its own (pharos-ui builds for
/// wasm and cannot depend on this crate); they must not drift.
#[cfg(test)]
mod wire_tests {
    use pharos_ui::native::ports::VisFrame;
    use pharos_ui::native::visualiser::wire;

    use super::*;
    use crate::spectrum::Analyser;

    #[test]
    fn the_pages_constants_are_ours() {
        assert_eq!(wire::DB_STEP, DB_STEP);
        assert_eq!(wire::SILENT_BIN, SILENT_BIN);
        assert_eq!(VisFrame::BINS, BINS);
        assert_eq!(VisFrame::silent(1).wave.len(), FFT_SIZE);
    }

    #[test]
    fn a_frame_reads_in_the_page_as_it_was_made() {
        let samples: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 * (std::f32::consts::TAU * 100.0 * i as f32 / FFT_SIZE as f32).sin())
            .collect();
        let frame = Analyser::new().frame(&samples);
        let (mut bins, mut wave) = (vec![0.0; BINS], vec![0.0; FFT_SIZE]);
        wire::dequantise(&frame.bins, &frame.wave, &mut bins, &mut wave);
        for (page, ours) in bins.iter().zip(&frame.bins) {
            assert_eq!(*page, dequantise_db(*ours));
        }
        for (page, ours) in wave.iter().zip(&frame.wave) {
            assert_eq!(*page, dequantise_sample(*ours));
        }
    }
}
