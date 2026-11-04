//! The music visualiser's decisions, away from the browser: which style is
//! chosen, and how an analyser's raw bins become the data texture a shader
//! samples (`render::DATA_WIDTH` × `render::DATA_ROWS`).

use crate::render::{Program, DATA_ROWS, DATA_WIDTH};

/// Where the chosen style is kept on this device.
pub const STYLE_KEY: &str = "pharos.visualiser";

/// Reads the playing media's sound (009 Boundaries: Web Audio sits behind
/// this port). Dropping it releases the audio graph.
pub trait AudioTap {
    /// Fill `bins` with each analyser bin's level in dBFS (`-inf` for none)
    /// and `wave` with the frame's time-domain samples (twice `bins` of
    /// them), from -1 to 1.
    fn read(&mut self, bins: &mut [f32], wave: &mut [f32]);
    /// Frequency bins the analyser produces.
    fn bins(&self) -> usize;
    fn sample_rate(&self) -> f32;
    /// The gain the player applied before the analyser: the media element's
    /// volume, 0 when muted. Chromium and Firefox both scale the analysed
    /// sound by it.
    fn volume(&self) -> f32;
}

/// A visualiser look, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    #[default]
    Spectrum,
    Scope,
    Halo,
    Plasma,
    Orbit,
    Horizon,
    Kaleidoscope,
    Starfield,
    Off,
}

impl Style {
    pub const ALL: [Style; 9] = [
        Style::Spectrum,
        Style::Scope,
        Style::Halo,
        Style::Plasma,
        Style::Orbit,
        Style::Horizon,
        Style::Kaleidoscope,
        Style::Starfield,
        Style::Off,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Style::Spectrum => "spectrum",
            Style::Scope => "scope",
            Style::Halo => "halo",
            Style::Plasma => "plasma",
            Style::Orbit => "orbit",
            Style::Horizon => "horizon",
            Style::Kaleidoscope => "kaleidoscope",
            Style::Starfield => "starfield",
            Style::Off => "off",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Style::Spectrum => "Spectrum",
            Style::Scope => "Scope",
            Style::Halo => "Halo",
            Style::Plasma => "Plasma",
            Style::Orbit => "Orbit",
            Style::Horizon => "Horizon",
            Style::Kaleidoscope => "Kaleidoscope",
            Style::Starfield => "Starfield",
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
            Style::Orbit => Some(Program::Orbit),
            Style::Horizon => Some(Program::Horizon),
            Style::Kaleidoscope => Some(Program::Kaleidoscope),
            Style::Starfield => Some(Program::Starfield),
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

/// Browsers analyse the sound after the media element's volume, so each
/// frame is first divided by that volume: the slider changes how loud the
/// song sounds, never how it looks, from the very next frame (B478).
/// Under this volume the element is near silent and nothing is undone.
const MIN_VOLUME: f32 = 1e-3;

/// A quietly mastered track would still look dim, so the analysis also
/// follows the music's own recent peak (an automatic gain): it jumps up to a
/// louder peak and relaxes over a few seconds.
///
/// Decibels shown below the reference peak.
const RANGE_DB: f32 = 70.0;
/// Where the reference peak sits under the top of the bars.
const HEADROOM_DB: f32 = 6.0;
/// The reference never drops below this, so the boost over a full-volume
/// master (`REFERENCE_START_DB`) is at most 40 dB, about a 1% slider. A
/// frame peaking under it is near-silence and shows no bars rather than
/// amplified noise.
const REFERENCE_FLOOR_DB: f32 = -70.0;
/// The reference before any sound, near a full-volume master's peak.
const REFERENCE_START_DB: f32 = -30.0;
/// Per frame: the gain follows a louder peak at once and a quieter one over
/// about four seconds at 60 frames a second.
const GAIN_ATTACK: f32 = 0.5;
const GAIN_RELEASE: f32 = 0.004;
/// The waveform's peak amplitude is followed the same way, down to this
/// floor (at most `WAVE_TARGET / PEAK_FLOOR`, 45×, boost).
const PEAK_FLOOR: f32 = 0.02;
/// Where the followed waveform peak lands, short of clipping.
const WAVE_TARGET: f32 = 0.9;
/// Samples averaged into each waveform texel. Picking one sample in eight
/// aliased the treble into spikes; four averaged keep up to ~6 kHz and
/// show the first ~21 ms of the frame (B479).
const WAVE_STEP: usize = 4;

/// Move `followed` towards `now`: quickly up, slowly down.
fn follow(followed: f32, now: f32) -> f32 {
    let rate = if now > followed {
        GAIN_ATTACK
    } else {
        GAIN_RELEASE
    };
    followed + (now - followed) * rate
}

/// Turns analyser frames into the shader's data texture and loudness.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// For each spectrum texel, the analyser bins it covers.
    ranges: Vec<(usize, usize)>,
    bands: Vec<f32>,
    level: f32,
    /// The followed spectrum peak, dBFS.
    reference_db: f32,
    /// The followed waveform peak amplitude.
    peak: f32,
    bins: Vec<f32>,
    wave: Vec<f32>,
    /// Each spectrum texel's loudest bin this frame, dBFS; reused per frame.
    texel_db: Vec<f32>,
    data: Vec<u8>,
}

impl Analysis {
    /// For an analyser with `bins` bins (and twice as many samples) at
    /// `sample_rate`; the spectrum row
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
            reference_db: REFERENCE_START_DB,
            peak: WAVE_TARGET,
            bins: vec![f32::NEG_INFINITY; bins],
            wave: vec![0.0; 2 * bins],
            texel_db: vec![f32::NEG_INFINITY; DATA_WIDTH],
            data,
        }
    }

    /// Read one frame from `tap` into the texture.
    pub fn read(&mut self, tap: &mut dyn AudioTap) {
        tap.read(&mut self.bins, &mut self.wave);
        let volume = tap.volume();
        if volume >= MIN_VOLUME {
            let db = 20.0 * volume.log10();
            self.bins.iter_mut().for_each(|b| *b -= db);
            self.wave.iter_mut().for_each(|s| *s /= volume);
        }
        self.update();
    }

    /// Where the waveform row starts: the first rising zero crossing that
    /// leaves a full row after it, so a steady tone draws in place rather
    /// than sliding between frames.
    fn trigger(&self) -> usize {
        let span = DATA_WIDTH * WAVE_STEP;
        let last = self.wave.len().saturating_sub(span);
        (1..=last)
            .find(|&i| self.wave[i - 1] < 0.0 && self.wave[i] >= 0.0)
            .unwrap_or(0)
    }

    /// Fold the current `bins` and `wave` into the texture.
    fn update(&mut self) {
        // Each texel shows the loudest bin it covers.
        for (texel, &(from, to)) in self.texel_db.iter_mut().zip(&self.ranges) {
            *texel = self.bins[from..to]
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max);
        }
        let frame_peak = self
            .texel_db
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        let audible = frame_peak >= REFERENCE_FLOOR_DB;
        if audible {
            self.reference_db = follow(self.reference_db, frame_peak).max(REFERENCE_FLOOR_DB);
        }
        let bottom = self.reference_db + HEADROOM_DB - RANGE_DB;
        for (band, db) in self.bands.iter_mut().zip(&self.texel_db) {
            let target = if audible && db.is_finite() {
                ((db - bottom) / RANGE_DB).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let rate = if target > *band { ATTACK } else { DECAY };
            *band += (target - *band) * rate;
        }

        let amplitude = self.wave.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
        if amplitude >= PEAK_FLOOR {
            self.peak = follow(self.peak, amplitude);
        }
        // The followed peak lags a sudden jump; this frame's own amplitude
        // bounds the gain so a transient is not clipped flat.
        let gain = WAVE_TARGET / self.peak.max(amplitude).max(PEAK_FLOOR);

        let start = self.trigger();
        let (spectrum, wave) = self.data.split_at_mut(DATA_WIDTH);
        for (texel, band) in spectrum.iter_mut().zip(&self.bands) {
            *texel = (band.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        let mut energy = 0.0;
        for (texel, window) in wave.iter_mut().zip(self.wave[start..].chunks(WAVE_STEP)) {
            let s = window.iter().sum::<f32>() / window.len() as f32;
            let s = (s * gain).clamp(-1.0, 1.0);
            energy += s * s;
            *texel = ((s * 0.5 + 0.5) * 255.0).round() as u8;
        }
        let rms = (energy / DATA_WIDTH as f32).sqrt();
        // Music peaking at WAVE_TARGET sits near 0.3 RMS; scale it near the top.
        let target = (rms * 2.8).min(1.0);
        let rate = if target > self.level { ATTACK } else { DECAY };
        self.level += (target - self.level) * rate;
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

    /// No sound in a bin.
    const NONE: f32 = f32::NEG_INFINITY;

    /// Analyser bins in every test, and the samples per frame.
    const BINS: usize = 1024;
    const SAMPLES: usize = 2 * BINS;

    struct Fixed {
        bins: Vec<f32>,
        wave: Vec<f32>,
        volume: f32,
    }

    impl AudioTap for Fixed {
        fn read(&mut self, bins: &mut [f32], wave: &mut [f32]) {
            bins.copy_from_slice(&self.bins);
            wave.copy_from_slice(&self.wave);
        }
        fn bins(&self) -> usize {
            self.bins.len()
        }
        fn sample_rate(&self) -> f32 {
            48_000.0
        }
        fn volume(&self) -> f32 {
            self.volume
        }
    }

    fn tap(bins: Vec<f32>, wave: Vec<f32>) -> (Analysis, Fixed) {
        let tap = Fixed {
            bins,
            wave,
            volume: 1.0,
        };
        (Analysis::new(tap.bins(), tap.sample_rate()), tap)
    }

    fn settle(a: &mut Analysis, t: &mut Fixed, frames: usize) {
        for _ in 0..frames {
            a.read(t);
        }
    }

    /// A song-like frame: a falling spectrum and a two-tone wave, scaled by
    /// `gain` (a quiet master, or the media element's slider).
    fn song(gain: f32) -> (Vec<f32>, Vec<f32>) {
        let db = 20.0 * gain.log10();
        let bins = (0..BINS)
            .map(|i| -20.0 - 40.0 * (i as f32 / BINS as f32) + db)
            .collect();
        let wave = (0..SAMPLES)
            .map(|i| {
                let x = i as f32 / SAMPLES as f32 * std::f32::consts::TAU;
                (0.6 * (3.0 * x).sin() + 0.3 * (17.0 * x).sin()) * gain
            })
            .collect();
        (bins, wave)
    }

    /// A sine of `period` samples starting at `phase` samples in.
    fn sine(amp: f32, period: f32, phase: f32) -> Vec<f32> {
        (0..SAMPLES)
            .map(|i| amp * ((i as f32 + phase) / period * std::f32::consts::TAU).sin())
            .collect()
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
        let (mut a, mut t) = tap(vec![NONE; BINS], vec![0.0; SAMPLES]);
        settle(&mut a, &mut t, 600);
        let data = a.data();
        assert!(data[..DATA_WIDTH].iter().all(|&b| b == 0));
        assert!(data[DATA_WIDTH..].iter().all(|&b| b == 128));
        assert_eq!(a.level(), 0.0);
    }

    #[test]
    fn a_quiet_master_looks_like_a_loud_one_once_the_gain_settles() {
        let (bins, wave) = song(1.0);
        let (mut loud, mut lt) = tap(bins, wave);
        let (bins, wave) = song(0.05);
        let (mut quiet, mut qt) = tap(bins, wave);
        settle(&mut loud, &mut lt, 2000);
        settle(&mut quiet, &mut qt, 2000);
        for (i, (l, q)) in loud.data().iter().zip(quiet.data()).enumerate() {
            assert!(l.abs_diff(*q) <= 3, "texel {i}: loud {l} quiet {q}");
        }
        assert!((loud.level() - quiet.level()).abs() < 0.02);
        assert!(loud.data()[..DATA_WIDTH].iter().any(|&b| b > 200));
    }

    #[test]
    fn noise_far_below_any_music_is_not_boosted_into_view() {
        let (mut a, mut t) = tap(vec![-120.0; BINS], vec![0.001; SAMPLES]);
        settle(&mut a, &mut t, 2000);
        assert!(a.data()[..DATA_WIDTH].iter().all(|&b| b == 0));
        assert!(a.data()[DATA_WIDTH..].iter().all(|&b| b.abs_diff(128) <= 6));
    }

    #[test]
    fn a_near_silent_element_between_the_floor_and_true_silence_shows_nothing() {
        // A slider near 0.1%: quieter than the boost reaches, louder than -inf.
        let (mut a, mut t) = tap(vec![-95.0; BINS], vec![0.0; SAMPLES]);
        settle(&mut a, &mut t, 2000);
        assert!(a.data()[..DATA_WIDTH].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_sudden_loud_wave_after_quiet_is_not_clipped() {
        let (mut a, mut t) = tap(vec![NONE; BINS], sine(0.1, SAMPLES as f32 / 2.0, 0.0));
        settle(&mut a, &mut t, 2000);
        t.wave = sine(0.9, SAMPLES as f32 / 2.0, 0.0);
        a.read(&mut t);
        let row = &a.data()[DATA_WIDTH..];
        let top = row.iter().filter(|&&b| b == 255).count();
        assert!(top <= 1, "{top} texels clipped at the top");
    }

    #[test]
    fn a_bass_tone_lights_the_left_of_the_spectrum_only() {
        // 1024 bins at 48 kHz: 23.4 Hz each, so bins 2..=4 hold ~50–100 Hz.
        let mut bins = vec![NONE; BINS];
        bins[2..=4].fill(-20.0);
        let (mut a, mut t) = tap(bins, vec![0.0; SAMPLES]);
        settle(&mut a, &mut t, 10);
        let (low, high) = a.data()[..DATA_WIDTH].split_at(DATA_WIDTH / 4);
        assert!(low.iter().any(|&b| b > 200), "{low:?}");
        assert!(high.iter().all(|&b| b == 0));
    }

    #[test]
    fn bars_rise_faster_than_they_fall() {
        let (mut a, mut t) = tap(vec![-20.0; BINS], vec![0.0; SAMPLES]);
        a.read(&mut t);
        let up = a.data()[DATA_WIDTH / 2];
        t.bins.fill(NONE);
        a.read(&mut t);
        let down = a.data()[DATA_WIDTH / 2];
        assert!(up > 140, "{up}");
        assert!(down > up / 2, "{up} {down}");
    }

    #[test]
    fn a_loud_wave_raises_the_level_and_keeps_its_shape() {
        // A square wave, 16 texels a cycle.
        let wave: Vec<f32> = (0..SAMPLES)
            .map(|i| if (i / 32) % 2 == 0 { -0.9 } else { 0.9 })
            .collect();
        let (mut a, mut t) = tap(vec![NONE; BINS], wave);
        settle(&mut a, &mut t, 10);
        assert!(a.level() > 0.9, "{}", a.level());
        let row = &a.data()[DATA_WIDTH..];
        assert!(
            row.iter().filter(|&&b| b < 20).count() > DATA_WIDTH / 3,
            "{row:?}"
        );
        assert!(
            row.iter().filter(|&&b| b > 235).count() > DATA_WIDTH / 3,
            "{row:?}"
        );
    }

    #[test]
    fn the_volume_slider_changes_nothing_from_the_next_frame() {
        let (bins, wave) = song(1.0);
        let (mut steady, mut st) = tap(bins.clone(), wave.clone());
        let (mut turned, mut tt) = tap(bins, wave);
        settle(&mut steady, &mut st, 300);
        settle(&mut turned, &mut tt, 300);
        for volume in [0.05, 1.0, 0.3] {
            let (bins, wave) = song(volume);
            tt.bins = bins;
            tt.wave = wave;
            tt.volume = volume;
            steady.read(&mut st);
            turned.read(&mut tt);
            for (i, (s, t)) in steady.data().iter().zip(turned.data()).enumerate() {
                assert!(s.abs_diff(*t) <= 1, "volume {volume} texel {i}: {s} vs {t}");
            }
            assert!((steady.level() - turned.level()).abs() < 0.01);
        }
    }

    #[test]
    fn a_muted_element_shows_nothing() {
        let (mut a, mut t) = tap(vec![NONE; BINS], vec![0.0; SAMPLES]);
        t.volume = 0.0;
        settle(&mut a, &mut t, 100);
        assert!(a.data()[..DATA_WIDTH].iter().all(|&b| b == 0));
        assert!(a.data()[DATA_WIDTH..].iter().all(|&b| b == 128));
    }

    #[test]
    fn a_steady_tone_draws_in_place_whatever_its_phase_in_the_frame() {
        let (mut a, mut t) = tap(vec![NONE; BINS], sine(0.5, 200.0, 0.0));
        settle(&mut a, &mut t, 200);
        let first = a.data()[DATA_WIDTH..].to_vec();
        for phase in [37.0, 101.0, 163.0] {
            t.wave = sine(0.5, 200.0, phase);
            a.read(&mut t);
            for (i, (x, y)) in first.iter().zip(&a.data()[DATA_WIDTH..]).enumerate() {
                assert!(x.abs_diff(*y) <= 4, "phase {phase} texel {i}: {x} vs {y}");
            }
        }
    }
}
