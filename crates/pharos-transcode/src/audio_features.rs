//! Tempo and energy of a song, from a short window of its audio (spec 024's
//! tempo and energy rules; ADR-0050).
//!
//! Pure DSP over mono `f32` PCM at [`ANALYSIS_RATE`]. It has no libav in it, so
//! it is unit-tested on synthetic signals without a decoder; the libav worker
//! (`libav::audio_features`) decodes the window and calls [`analyse`].
//!
//! # Why a window and not the whole track
//!
//! The cost of analysing a song is almost all decode and NFS read, not maths:
//! the maths below over 30 s of 11 kHz mono is about 330k samples and runs in a
//! few milliseconds. So the efficiency lever is how much is decoded. Thirty
//! seconds from a third of the way in is past most intros and holds a dozen or
//! more bars at any tempo a mix rule cares about, and it reads roughly a tenth
//! of a typical song.
//!
//! # Tempo
//!
//! The classic onset-autocorrelation estimator (Scheirer 1998; Ellis 2007, as
//! in librosa's `beat.tempo`):
//!
//! 1. Short-time log energy every [`HOP`] samples (5.8 ms) in two bands, the
//!    whole signal and a first-difference high-pass (hats and snares carry the
//!    beat in most mixes, kick drums in the rest).
//! 2. The onset envelope is the half-wave-rectified rise of each band, summed.
//! 3. It is smoothed over ~30 ms, and its mean-removed autocorrelation is
//!    taken over lags from [`MAX_BPM`] down to [`MIN_BPM`]. Each lag scores its
//!    own autocorrelation plus half the autocorrelation at half the lag (a beat
//!    is subdivided; a lag of 1.5 beats lines up a kick with a hat but its half
//!    lines up nothing, which is what stops 174 BPM reading as 116), weighted
//!    by a log-normal prior around [`PRIOR_BPM`], one octave wide. The prior is
//!    what settles the octave a steady pulse leaves open: to an
//!    autocorrelation a beat every 0.345 s is 174 BPM or 87, and the centre
//!    sits so a drum-machine beat reads at its own tempo from 72 to 174 BPM,
//!    which is how such music is tagged. Outside that range a reading can be
//!    an octave off (60 reads as 120, 190 as 95), as with any such estimator.
//! 4. The best lag is refined by parabolic interpolation.
//!
//! The peak's height over the zero-lag value is the confidence. Below
//! [`MIN_TEMPO_CONFIDENCE`] (ambient, rubato, spoken word) the tempo is left
//! unknown rather than guessed, because a mix rule filtering on a made-up
//! number is worse than one that skips the song.
//!
//! # Energy
//!
//! A 0.0 (calm) to 1.0 (intense) blend of three things a listener hears as
//! intensity: loudness (RMS in dBFS, mapped from -32 to -8), brightness (the
//! high-pass share of the RMS) and onset density (rises per second). Weighted
//! 0.5 / 0.3 / 0.2. It is a ranking signal for "more like this" and "calm to
//! intense" rules, not a calibrated measurement, and [`ANALYSIS_VERSION`]
//! changes when the formula does so stored values are recomputed.

pub use pharos_core::TempoSource;
use serde::{Deserialize, Serialize};

/// Sample rate the window is resampled to before analysis. 11 025 Hz keeps
/// everything below 5.5 kHz, which carries the beat, at a quarter of the work
/// of CD audio.
pub const ANALYSIS_RATE: u32 = 11_025;

/// How much of a song is decoded.
pub const WINDOW_MS: u64 = 30_000;

/// Bumped whenever the decode window or a formula here changes, so a stored
/// result from an older analyser is recomputed (V385).
///
/// 2: the window is measured from the stream's own origin (B445), so a
/// song analysed at 1 with a non-zero start time is redone.
pub const ANALYSIS_VERSION: u32 = 2;

/// Samples between envelope frames (5.8 ms at [`ANALYSIS_RATE`]).
const HOP: usize = 64;
/// Samples per energy frame (23 ms).
const FRAME: usize = 256;
/// Slowest and fastest tempo considered.
pub const MIN_BPM: f32 = 50.0;
pub const MAX_BPM: f32 = 220.0;
/// Centre of the tempo prior. Above 120 on purpose: see the module docs; the
/// test `a_steady_beat_reads_at_its_own_tempo` pins the range it buys.
const PRIOR_BPM: f32 = 135.0;
/// Below this normalised autocorrelation peak the tempo is unknown.
pub const MIN_TEMPO_CONFIDENCE: f32 = 0.08;
/// Shortest window worth analysing: eight beats at the slowest tempo.
const MIN_SECONDS: f32 = 10.0;

/// What the worker reports for one song.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioFeatures {
    /// Beats per minute from the file's BPM tag, when it has a usable one.
    pub tag_bpm: Option<f32>,
    /// Beats per minute from the audio. `None` when a tag was present (the
    /// estimator is skipped) or the estimate was not confident.
    pub analysed_bpm: Option<f32>,
    /// Normalised autocorrelation peak behind `analysed_bpm` (0 when the
    /// estimator did not run). Logged so the threshold can be tuned by query.
    pub tempo_confidence: f32,
    /// 0.0 calm to 1.0 intense. `None` only when the window was too short or
    /// silent.
    pub energy: Option<f32>,
    /// Seconds of audio actually analysed.
    pub analysed_seconds: f32,
}

impl AudioFeatures {
    /// The tempo a mix rule should use: the tag when present, else analysis.
    pub fn bpm(&self) -> Option<(f32, TempoSource)> {
        self.tag_bpm
            .map(|b| (b, TempoSource::Tag))
            .or(self.analysed_bpm.map(|b| (b, TempoSource::Analysis)))
    }
}

/// Parse a BPM tag value: `128`, `128.00`, `127,5`, `128 BPM`. Values outside
/// 20 to 300, and `0` (which some taggers write for "unknown"), are refused.
pub fn parse_bpm_tag(raw: &str) -> Option<f32> {
    let t = raw.trim();
    let end = t
        .char_indices()
        .find(|&(_, c)| !(c.is_ascii_digit() || c == '.' || c == ','))
        .map_or(t.len(), |(i, _)| i);
    let v: f32 = t[..end].replace(',', ".").parse().ok()?;
    (20.0..=300.0).contains(&v).then_some(v)
}

/// Where in a song of `duration_ms` to start the window, and how long it is.
/// A third of the way in, clamped so the window ends inside the song; a song
/// shorter than the window is read whole.
pub fn window_for(duration_ms: Option<u64>) -> (u64, u64) {
    match duration_ms {
        Some(d) if d > WINDOW_MS => {
            let start = (d / 3).min(d - WINDOW_MS);
            (start, WINDOW_MS)
        }
        Some(d) if d > 0 => (0, d),
        // Unknown length: read from the top.
        _ => (0, WINDOW_MS),
    }
}

/// Analyse mono PCM at [`ANALYSIS_RATE`]. `skip_tempo` is set when the file
/// carries a BPM tag, so only energy is computed.
pub fn analyse(samples: &[f32], skip_tempo: bool) -> AudioFeatures {
    let seconds = samples.len() as f32 / ANALYSIS_RATE as f32;
    let mut out = AudioFeatures {
        analysed_seconds: seconds,
        ..AudioFeatures::default()
    };
    if seconds < MIN_SECONDS || samples.len() < FRAME * 4 {
        return out;
    }

    let (full, high) = band_energies(samples);
    let onset = onset_envelope(&full, &high);

    out.energy = energy(samples, &onset);
    if !skip_tempo {
        if let Some((bpm, conf)) = tempo(&onset) {
            out.tempo_confidence = conf;
            if conf >= MIN_TEMPO_CONFIDENCE {
                out.analysed_bpm = Some(bpm);
            }
        }
    }
    out
}

/// Mean-square energy per frame for the whole signal and for its first
/// difference, one frame every [`HOP`] samples, from prefix sums.
fn band_energies(x: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let n = x.len();
    let mut pf = Vec::with_capacity(n + 1);
    let mut ph = Vec::with_capacity(n + 1);
    pf.push(0.0f64);
    ph.push(0.0f64);
    let mut prev = 0.0f32;
    for &s in x {
        let d = s - prev;
        prev = s;
        let lf = pf.last().copied().unwrap_or(0.0);
        let lh = ph.last().copied().unwrap_or(0.0);
        pf.push(lf + f64::from(s) * f64::from(s));
        ph.push(lh + f64::from(d) * f64::from(d));
    }
    let frames = (n - FRAME) / HOP + 1;
    let mut full = Vec::with_capacity(frames);
    let mut high = Vec::with_capacity(frames);
    for i in 0..frames {
        let (a, b) = (i * HOP, i * HOP + FRAME);
        full.push(((pf[b] - pf[a]) / FRAME as f64) as f32);
        high.push(((ph[b] - ph[a]) / FRAME as f64) as f32);
    }
    (full, high)
}

/// Half-wave-rectified rise of log-compressed band energy, summed over bands.
fn onset_envelope(full: &[f32], high: &[f32]) -> Vec<f32> {
    // log(1 + C·E): C sets where compression starts. 1e4 puts it near -40
    // dBFS, below the floor of anything mastered.
    const C: f32 = 1.0e4;
    let lf: Vec<f32> = full.iter().map(|&e| (C * e).ln_1p()).collect();
    let lh: Vec<f32> = high.iter().map(|&e| (C * e).ln_1p()).collect();
    let mut o = Vec::with_capacity(lf.len());
    o.push(0.0);
    for i in 1..lf.len() {
        o.push((lf[i] - lf[i - 1]).max(0.0) + (lh[i] - lh[i - 1]).max(0.0));
    }
    o
}

/// Envelope frames per second.
fn frame_rate() -> f32 {
    ANALYSIS_RATE as f32 / HOP as f32
}

/// `(bpm, confidence)` of the best-scoring beat period, or `None` when the
/// envelope is flat.
fn tempo(onset: &[f32]) -> Option<(f32, f32)> {
    // Smooth over ~30 ms first. Onsets are a frame or two wide and a beat is
    // rarely a whole number of frames, so unsmoothed peaks miss each other by
    // a frame at the true lag and the autocorrelation goes ragged.
    const K: [f32; 5] = [1.0, 2.0, 3.0, 2.0, 1.0];
    let n = onset.len();
    let smooth: Vec<f32> = (0..n)
        .map(|i| {
            K.iter()
                .enumerate()
                .filter_map(|(k, w)| {
                    (i + k)
                        .checked_sub(2)
                        .and_then(|j| onset.get(j))
                        .map(|v| v * w)
                })
                .sum::<f32>()
                / 9.0
        })
        .collect();
    let mean = smooth.iter().sum::<f32>() / n as f32;
    let o: Vec<f32> = smooth.iter().map(|v| v - mean).collect();
    let fps = frame_rate();
    let min_lag = (60.0 * fps / MAX_BPM).floor() as usize;
    let max_lag = (60.0 * fps / MIN_BPM).ceil() as usize;
    if o.len() <= 2 * max_lag {
        return None;
    }
    let ac = |lag: usize| -> f32 {
        let n = o.len() - lag;
        let mut s = 0.0f64;
        for i in 0..n {
            s += f64::from(o[i]) * f64::from(o[i + lag]);
        }
        (s / n as f64) as f32
    };
    let r0 = ac(0);
    if r0 <= f32::EPSILON {
        return None;
    }
    // Normalised autocorrelation for every lag the scoring reads: the lags
    // themselves, their halves, and `max_lag` for the parabolic fit.
    let r: Vec<f32> = (0..=max_lag).map(|l| ac(l) / r0).collect();
    // A beat is subdivided, so half its lag repeats too. A lag of 1.5 beats
    // lines a kick up with a hat, but its half lines up nothing, which is what
    // keeps 174 BPM from reading as 116.
    let half = |lag: usize| -> f32 {
        if lag % 2 == 0 {
            r[lag / 2]
        } else {
            0.5 * (r[lag / 2] + r[lag / 2 + 1])
        }
    };
    let score = |lag: usize| -> f32 {
        let bpm = 60.0 * fps / lag as f32;
        let oct = (bpm / PRIOR_BPM).log2();
        let prior = (-0.5 * oct * oct).exp();
        (r[lag] + 0.5 * half(lag)).max(0.0) * prior
    };
    let mut best = min_lag;
    let mut best_score = f32::MIN;
    for lag in min_lag..=max_lag {
        let s = score(lag);
        if s > best_score {
            best_score = s;
            best = lag;
        }
    }
    // Parabolic refinement on the raw autocorrelation around the peak.
    let lag = if best > min_lag && best < max_lag {
        let (a, b, c) = (r[best - 1], r[best], r[best + 1]);
        let denom = a - 2.0 * b + c;
        if denom.abs() > f32::EPSILON {
            best as f32 + (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
        } else {
            best as f32
        }
    } else {
        best as f32
    };
    let bpm = 60.0 * fps / lag;
    Some(((bpm * 10.0).round() / 10.0, r[best].clamp(0.0, 1.0)))
}

/// 0.0 to 1.0 from loudness, brightness and onset density. `None` for
/// digital silence.
fn energy(x: &[f32], onset: &[f32]) -> Option<f32> {
    let n = x.len() as f64;
    let ms: f64 = x.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / n;
    if ms <= 1e-12 {
        return None;
    }
    let rms = ms.sqrt();
    let db = 20.0 * rms.log10();
    let loud = ((db + 32.0) / 24.0).clamp(0.0, 1.0) as f32;

    let mut prev = 0.0f32;
    let hs: f64 = x
        .iter()
        .map(|&s| {
            let d = s - prev;
            prev = s;
            f64::from(d) * f64::from(d)
        })
        .sum::<f64>()
        / n;
    // A first difference passes white noise at √2 of its RMS and a 100 Hz
    // sine at about 0.06 of it at this rate; 0.6 and above reads as bright.
    let bright = ((hs.sqrt() / rms) / 0.6).clamp(0.0, 1.0) as f32;

    // Onsets per second: local maxima of the envelope that stand above its
    // spread AND rise by a real amount (0.3 in log energy is about a 1.3 dB
    // jump), so the numerical ripple of a held note never counts.
    let len = onset.len() as f32;
    let mean = onset.iter().sum::<f32>() / len;
    let sd = (onset.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / len).sqrt();
    let floor = (mean + sd).max(0.3);
    let peaks = onset
        .windows(3)
        .filter(|w| w[1] > w[0] && w[1] >= w[2] && w[1] > floor)
        .count();
    let rate = peaks as f32 / (x.len() as f32 / ANALYSIS_RATE as f32);
    let dense = (rate / 8.0).clamp(0.0, 1.0);

    Some(((0.5 * loud + 0.3 * bright + 0.2 * dense) * 1000.0).round() / 1000.0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const SR: f32 = ANALYSIS_RATE as f32;

    /// A deterministic white-noise source, so the tests never flake.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    /// A drum-machine-ish beat: a decaying low thump on every beat and a
    /// short noise hat on every off-beat, `seconds` long.
    fn beat(bpm: f32, seconds: f32, gain: f32) -> Vec<f32> {
        let n = (seconds * SR) as usize;
        let period = 60.0 / bpm * SR;
        let mut rng = Lcg(7);
        let mut x = vec![0.0f32; n];
        let mut t = 0.0f32;
        while (t as usize) < n {
            let start = t as usize;
            for i in 0..(0.12 * SR) as usize {
                if start + i >= n {
                    break;
                }
                let tt = i as f32 / SR;
                x[start + i] +=
                    gain * (-tt * 30.0).exp() * (2.0 * std::f32::consts::PI * 60.0 * tt).sin();
            }
            let off = (t + period / 2.0) as usize;
            for i in 0..(0.03 * SR) as usize {
                if off + i >= n {
                    break;
                }
                let tt = i as f32 / SR;
                x[off + i] += 0.4 * gain * (-tt * 120.0).exp() * rng.next();
            }
            t += period;
        }
        x
    }

    fn assert_bpm(bpm: f32) {
        let f = analyse(&beat(bpm, 30.0, 0.8), false);
        let got = f.analysed_bpm.unwrap_or_else(|| {
            panic!(
                "{bpm} BPM beat gave no tempo (confidence {})",
                f.tempo_confidence
            )
        });
        assert!(
            (got - bpm).abs() <= 1.0,
            "{bpm} BPM beat read as {got} (confidence {})",
            f.tempo_confidence
        );
    }

    #[test]
    fn a_steady_beat_reads_at_its_own_tempo() {
        for bpm in [
            72.0, 85.0, 90.0, 100.0, 110.0, 120.0, 128.0, 140.0, 150.0, 160.0,
        ] {
            assert_bpm(bpm);
        }
    }

    #[test]
    fn drum_and_bass_reads_fast_not_half_time() {
        // The octave a steady pulse leaves open is settled by the prior.
        assert_bpm(174.0);
    }

    #[test]
    fn far_outside_the_usual_range_a_tempo_is_at_worst_an_octave_off() {
        for bpm in [60.0f32, 190.0] {
            let got = analyse(&beat(bpm, 30.0, 0.8), false).analysed_bpm.unwrap();
            let ratio = got / bpm;
            let octave = [0.5f32, 1.0, 2.0].iter().any(|o| (ratio - o).abs() < 0.02);
            assert!(octave, "{bpm} BPM read as {got}, not an octave of it");
        }
    }

    #[test]
    fn noise_has_no_tempo() {
        let mut rng = Lcg(3);
        let x: Vec<f32> = (0..(30.0 * SR) as usize)
            .map(|_| 0.3 * rng.next())
            .collect();
        let f = analyse(&x, false);
        assert_eq!(f.analysed_bpm, None, "confidence {}", f.tempo_confidence);
        assert!(f.tempo_confidence < MIN_TEMPO_CONFIDENCE);
    }

    #[test]
    fn a_tag_skips_the_tempo_estimate_but_not_energy() {
        let f = analyse(&beat(128.0, 30.0, 0.8), true);
        assert_eq!(f.analysed_bpm, None);
        assert_eq!(f.tempo_confidence, 0.0);
        assert!(f.energy.is_some());
    }

    #[test]
    fn a_quiet_pad_is_calmer_than_a_loud_beat() {
        let pad: Vec<f32> = (0..(30.0 * SR) as usize)
            .map(|i| 0.03 * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / SR).sin())
            .collect();
        let calm = analyse(&pad, false).energy.unwrap();
        let mut rng = Lcg(11);
        let wall: Vec<f32> = beat(140.0, 30.0, 0.9)
            .into_iter()
            .map(|s| (s + 0.25 * rng.next()).clamp(-1.0, 1.0))
            .collect();
        let intense = analyse(&wall, false).energy.unwrap();
        assert!(calm < 0.25, "pad energy {calm}");
        assert!(intense > 0.6, "loud beat energy {intense}");
        assert!((0.0..=1.0).contains(&calm) && (0.0..=1.0).contains(&intense));
    }

    #[test]
    fn silence_and_short_windows_report_nothing() {
        let f = analyse(&vec![0.0; (30.0 * SR) as usize], false);
        assert_eq!((f.energy, f.analysed_bpm), (None, None));
        let f = analyse(&beat(120.0, 5.0, 0.8), false);
        assert_eq!((f.energy, f.analysed_bpm), (None, None));
        assert!((f.analysed_seconds - 5.0).abs() < 0.01);
    }

    #[test]
    fn bpm_tags_parse_the_forms_taggers_write() {
        assert_eq!(parse_bpm_tag("128"), Some(128.0));
        assert_eq!(parse_bpm_tag(" 127.50 "), Some(127.5));
        assert_eq!(parse_bpm_tag("127,5"), Some(127.5));
        assert_eq!(parse_bpm_tag("174 BPM"), Some(174.0));
        assert_eq!(parse_bpm_tag("0"), None, "0 means unknown to many taggers");
        assert_eq!(parse_bpm_tag("999"), None);
        assert_eq!(parse_bpm_tag("fast"), None);
        assert_eq!(parse_bpm_tag(""), None);
    }

    #[test]
    fn the_window_sits_a_third_in_and_ends_inside_the_song() {
        assert_eq!(window_for(Some(240_000)), (80_000, WINDOW_MS));
        assert_eq!(window_for(Some(40_000)), (10_000, WINDOW_MS));
        assert_eq!(window_for(Some(20_000)), (0, 20_000));
        assert_eq!(window_for(None), (0, WINDOW_MS));
        assert_eq!(window_for(Some(0)), (0, WINDOW_MS));
    }

    #[test]
    fn the_tag_wins_over_analysis() {
        let f = AudioFeatures {
            tag_bpm: Some(120.0),
            analysed_bpm: Some(60.0),
            ..Default::default()
        };
        assert_eq!(f.bpm(), Some((120.0, TempoSource::Tag)));
        let f = AudioFeatures {
            analysed_bpm: Some(60.0),
            ..Default::default()
        };
        assert_eq!(f.bpm(), Some((60.0, TempoSource::Analysis)));
    }
}
