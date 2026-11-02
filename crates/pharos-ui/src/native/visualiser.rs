//! The music visualiser's decisions, away from the browser: which style is
//! chosen, and how an analyser's raw bins become the data texture a shader
//! samples (`render::DATA_WIDTH` × `render::DATA_ROWS`).

use crate::render::{Program, DATA_ROWS, DATA_WIDTH};

/// Where the chosen style is kept on this device.
pub const STYLE_KEY: &str = "pharos.visualiser";

/// Reads the playing media's sound (009 Boundaries: Web Audio sits behind
/// this port). Dropping it releases the audio graph.
pub trait AudioTap {
    /// Fill `bins` with the frequency magnitudes (0–255, one per analyser
    /// bin) and `wave` with time-domain samples (128 is silence).
    fn read(&mut self, bins: &mut [u8], wave: &mut [u8]);
    /// Frequency bins the analyser produces.
    fn bins(&self) -> usize;
    fn sample_rate(&self) -> f32;
}

/// A visualiser look, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    #[default]
    Spectrum,
    Scope,
    Halo,
    Plasma,
    Off,
}

impl Style {
    pub const ALL: [Style; 5] = [
        Style::Spectrum,
        Style::Scope,
        Style::Halo,
        Style::Plasma,
        Style::Off,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Style::Spectrum => "spectrum",
            Style::Scope => "scope",
            Style::Halo => "halo",
            Style::Plasma => "plasma",
            Style::Off => "off",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Style::Spectrum => "Spectrum",
            Style::Scope => "Scope",
            Style::Halo => "Halo",
            Style::Plasma => "Plasma",
            Style::Off => "Off",
        }
    }

    /// The stored choice, the default when it is missing or unknown.
    pub fn parse(stored: Option<&str>) -> Style {
        Style::ALL
            .into_iter()
            .find(|s| Some(s.label()) == stored)
            .unwrap_or_default()
    }

    pub fn program(self) -> Option<Program> {
        match self {
            Style::Spectrum => Some(Program::Spectrum),
            Style::Scope => Some(Program::Scope),
            Style::Halo => Some(Program::Halo),
            Style::Plasma => Some(Program::Plasma),
            Style::Off => None,
        }
    }
}

/// The lowest and highest frequencies the spectrum row spans: below 30 Hz
/// is rumble, above 16 kHz most lossy encodes are empty.
const LOW_HZ: f32 = 30.0;
const HIGH_HZ: f32 = 16_000.0;
/// Per-frame smoothing: bars jump up quickly and fall back slowly, so beats
/// read as hits rather than flicker.
const ATTACK: f32 = 0.6;
const DECAY: f32 = 0.12;

/// Turns analyser frames into the shader's data texture and loudness.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// For each spectrum texel, the analyser bins it averages.
    ranges: Vec<(usize, usize)>,
    bands: Vec<f32>,
    level: f32,
    bins: Vec<u8>,
    wave: Vec<u8>,
    data: Vec<u8>,
}

impl Analysis {
    /// For an analyser with `bins` bins at `sample_rate`; the spectrum row
    /// is spaced logarithmically so each octave gets equal width.
    pub fn new(bins: usize, sample_rate: f32) -> Self {
        let nyquist = sample_rate / 2.0;
        let hz_per_bin = nyquist / bins.max(1) as f32;
        let high = HIGH_HZ.min(nyquist);
        let ranges = (0..DATA_WIDTH)
            .map(|i| {
                let at = |t: f32| LOW_HZ * (high / LOW_HZ).powf(t) / hz_per_bin;
                let from = at(i as f32 / DATA_WIDTH as f32) as usize;
                let to = (at((i + 1) as f32 / DATA_WIDTH as f32) as usize).max(from + 1);
                (from.min(bins), to.min(bins))
            })
            .collect();
        let mut data = vec![0; DATA_WIDTH * DATA_ROWS];
        data[DATA_WIDTH..].fill(128);
        Self {
            ranges,
            bands: vec![0.0; DATA_WIDTH],
            level: 0.0,
            bins: vec![0; bins],
            wave: vec![128; DATA_WIDTH],
            data,
        }
    }

    /// Read one frame from `tap` into the texture.
    pub fn read(&mut self, tap: &mut dyn AudioTap) {
        tap.read(&mut self.bins, &mut self.wave);
        self.update();
    }

    /// Fold the current `bins` and `wave` into the texture.
    fn update(&mut self) {
        for (band, &(from, to)) in self.bands.iter_mut().zip(&self.ranges) {
            let slice = &self.bins[from..to];
            let target = if slice.is_empty() {
                0.0
            } else {
                slice.iter().map(|&b| f32::from(b)).sum::<f32>() / (slice.len() as f32 * 255.0)
            };
            let rate = if target > *band { ATTACK } else { DECAY };
            *band += (target - *band) * rate;
        }
        let rms = (self
            .wave
            .iter()
            .map(|&s| (f32::from(s) - 128.0) / 128.0)
            .map(|s| s * s)
            .sum::<f32>()
            / self.wave.len().max(1) as f32)
            .sqrt();
        // Loud masters sit near 0.35 RMS; scale so they reach the top.
        let target = (rms * 2.8).min(1.0);
        let rate = if target > self.level { ATTACK } else { DECAY };
        self.level += (target - self.level) * rate;

        let (spectrum, wave) = self.data.split_at_mut(DATA_WIDTH);
        for (texel, band) in spectrum.iter_mut().zip(&self.bands) {
            *texel = (band.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        wave.copy_from_slice(&self.wave);
    }

    /// The texture bytes, spectrum row then waveform row.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Smoothed loudness, 0 to 1.
    pub fn level(&self) -> f32 {
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        bins: Vec<u8>,
        wave: Vec<u8>,
    }

    impl AudioTap for Fixed {
        fn read(&mut self, bins: &mut [u8], wave: &mut [u8]) {
            bins.copy_from_slice(&self.bins);
            wave.copy_from_slice(&self.wave);
        }
        fn bins(&self) -> usize {
            self.bins.len()
        }
        fn sample_rate(&self) -> f32 {
            48_000.0
        }
    }

    fn tap(bins: Vec<u8>, wave: Vec<u8>) -> (Analysis, Fixed) {
        let tap = Fixed { bins, wave };
        (Analysis::new(tap.bins(), tap.sample_rate()), tap)
    }

    #[test]
    fn styles_round_trip_and_unknown_ones_fall_back() {
        for style in Style::ALL {
            assert_eq!(Style::parse(Some(style.label())), style);
        }
        assert_eq!(Style::parse(Some("milkdrop")), Style::Spectrum);
        assert_eq!(Style::parse(None), Style::Spectrum);
        assert_eq!(Style::Off.program(), None);
    }

    #[test]
    fn silence_is_an_empty_spectrum_and_a_flat_wave() {
        let (mut a, mut t) = tap(vec![0; 1024], vec![128; DATA_WIDTH]);
        a.read(&mut t);
        let data = a.data();
        assert!(data[..DATA_WIDTH].iter().all(|&b| b == 0));
        assert!(data[DATA_WIDTH..].iter().all(|&b| b == 128));
        assert_eq!(a.level(), 0.0);
    }

    #[test]
    fn a_bass_tone_lights_the_left_of_the_spectrum_only() {
        // 1024 bins at 48 kHz: 23.4 Hz each, so bins 2..=4 hold ~50–100 Hz.
        let mut bins = vec![0; 1024];
        bins[2..=4].fill(255);
        let (mut a, mut t) = tap(bins, vec![128; DATA_WIDTH]);
        for _ in 0..10 {
            a.read(&mut t);
        }
        let (low, high) = a.data()[..DATA_WIDTH].split_at(DATA_WIDTH / 4);
        assert!(low.iter().any(|&b| b > 200), "{low:?}");
        assert!(high.iter().all(|&b| b == 0));
    }

    #[test]
    fn bars_rise_faster_than_they_fall() {
        let (mut a, mut t) = tap(vec![255; 1024], vec![128; DATA_WIDTH]);
        a.read(&mut t);
        let up = a.data()[DATA_WIDTH / 2];
        t.bins.fill(0);
        a.read(&mut t);
        let down = a.data()[DATA_WIDTH / 2];
        assert!(up > 140, "{up}");
        assert!(down > up / 2, "{up} {down}");
    }

    #[test]
    fn a_loud_wave_raises_the_level_and_passes_through() {
        let wave: Vec<u8> = (0..DATA_WIDTH)
            .map(|i| if i % 2 == 0 { 30 } else { 226 })
            .collect();
        let (mut a, mut t) = tap(vec![0; 1024], wave.clone());
        for _ in 0..10 {
            a.read(&mut t);
        }
        assert!(a.level() > 0.9, "{}", a.level());
        assert_eq!(&a.data()[DATA_WIDTH..], wave.as_slice());
    }
}
