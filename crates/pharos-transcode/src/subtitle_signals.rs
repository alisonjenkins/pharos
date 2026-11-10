//! Per-subtitle-line audio signals and scene-cut proxy (spec 028, FR-009,
//! FR-012, FR-013).
//!
//! Pure DSP over mono `f32` PCM at [`ANALYSIS_RATE`], with no libav in it, so
//! it is unit-tested on synthetic signals. The libav worker
//! (`libav::subtitle_signals`) decodes the file once and feeds the samples to a
//! [`SignalAnalyser`].
//!
//! # One linear pass
//!
//! [`SignalAnalyser::push`] runs each sample through four biquads once and
//! reduces it to one small record per 112-sample block (10.2 ms): the sum of
//! squares after K-weighting, after a 3.4 kHz high-pass, and unweighted, the
//! peak, and the sample count. It also keeps a 4x decimated copy for pitch.
//! [`SignalAnalyser::finish`] then answers every line from those records, so a
//! line costs its own span and never a rescan of the file. Lines may overlap.
//! Pitch frames are computed lazily and memoised, only for frames a line
//! covers.
//!
//! # Loudness
//!
//! ITU-R BS.1770-4 / EBU R128 K-weighting: a high shelf (+4 dB above ~1.7 kHz)
//! then the 38 Hz RLB high-pass, as the two biquads' analogue prototypes
//! (f0, Q, gain) mapped to the analysis rate with the bilinear transform. The
//! standard's 48 kHz coefficient table is not used: it is wrong at 11 025 Hz.
//! A line's loudness is `-0.691 + 10 log10(mean square)` over its span,
//! with a span under 400 ms widened to 400 ms centred on it (the R128
//! momentary block). This is deliberately not the 3 s short-term window: a
//! line is a few seconds at most, and a 3 s window around a one-second shout
//! would average in its neighbours and flatten exactly the contrast the
//! director looks for. One channel carries the signal, so its weight is 1.
//!
//! # Pitch
//!
//! YIN (de Cheveigne and Kawahara 2002). The signal is low-passed at 1 kHz and
//! decimated by 4 (2 756 Hz), which keeps the fundamental of any voice and
//! cuts the difference function from 157 lags x 441 samples to 39 x 110 per
//! frame, the difference between ~3 s and ~0.2 s CPU for a 24-minute episode.
//! Frames are 40 ms, hop ~10 ms, search 70-500 Hz. A frame is voiced when the
//! cumulative-mean-normalised difference dips below [`YIN_THRESHOLD`]; the
//! estimate is the dip's parabolic-interpolated lag.
//!
//! # Cuts
//!
//! See [`cuts_from_keyframes`].

pub use crate::audio_features::ANALYSIS_RATE;
use serde::{Deserialize, Serialize};

/// Samples per loudness/high-band/peak block (10.2 ms at 11 025 Hz). A
/// multiple of [`DECIM`] so the pitch signal stays aligned to the blocks.
const BLOCK: usize = 112;
/// Decimation factor of the pitch signal.
const DECIM: usize = 4;
/// Sample rate of the pitch signal.
const YIN_RATE: f64 = ANALYSIS_RATE as f64 / DECIM as f64;
/// YIN frame: 110 samples at 2 756 Hz is 39.9 ms.
const YIN_FRAME: usize = 110;
/// YIN hop: 28 samples is 10.2 ms (the block length).
const YIN_HOP: usize = BLOCK / DECIM;
/// Longest lag: 70 Hz.
const TAU_MAX: usize = 39;
/// Shortest lag searched: 5 samples is 551 Hz; interpolated results above
/// 500 Hz are rejected.
const TAU_MIN: usize = 5;
const F0_MIN: f64 = 70.0;
const F0_MAX: f64 = 500.0;
/// A cumulative-mean-normalised difference below this is voiced.
pub const YIN_THRESHOLD: f32 = 0.15;
/// Mean square below this (-70 dBFS) is silence, never voiced.
const YIN_SILENCE: f32 = 1e-7;
/// A line shorter than this is measured over a window of this length.
const MIN_WINDOW_MS: u64 = 400;
/// Loudness and peak floor reported for digital silence.
const FLOOR_DB: f32 = -120.0;
/// Fewest voiced frames for a median pitch.
const MIN_VOICED_FRAMES: usize = 3;
/// High-band cut-off.
const HIGH_BAND_HZ: f64 = 3_400.0;

/// Which samples the analysis heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelPick {
    /// The front-centre channel of a 5.1/7.1 layout, where dialogue sits.
    FrontCentre,
    /// All channels mixed down to mono.
    Downmix,
}

/// Audio signals of one subtitle line's span. `None` when the span has no
/// audio at all (past the end of the stream).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LineSignals {
    /// EBU R128 loudness of the span, LUFS (floored at -120 for silence).
    pub loudness_lufs: Option<f32>,
    /// Sample peak of the span, dBFS (floored at -120 for silence).
    pub peak_dbfs: Option<f32>,
    /// Median F0 of the voiced frames, Hz; `None` under three voiced frames.
    pub pitch_hz: Option<f32>,
    /// Voiced frames over frames in the span, 0.0 when there are none.
    pub voiced_ratio: f32,
    /// Share of the span's energy above 3.4 kHz, 0.0-1.0.
    pub high_band_share: Option<f32>,
}

impl LineSignals {
    /// A span with no audio.
    pub const NONE: LineSignals = LineSignals {
        loudness_lufs: None,
        peak_dbfs: None,
        pitch_hz: None,
        voiced_ratio: 0.0,
        high_band_share: None,
    };
}

/// Bumped whenever this module or the libav pass would measure the same
/// audio or video differently. Stored passes carry it, so an older one is
/// measured again (spec 028, B1202).
pub const SIGNALS_VERSION: u32 = 2;

/// Everything one `SubtitleSignals` op reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleSignals {
    /// One entry per requested window, in order.
    pub lines: Vec<LineSignals>,
    /// Every video keyframe, ms from the start of the file, ascending.
    pub keyframes_ms: Vec<u64>,
    /// The keyframes that are likely scene cuts ([`cuts_from_keyframes`]).
    pub cuts_ms: Vec<u64>,
    /// Per requested window: the height of text burned into the bottom of
    /// the picture under it, as a share of the frame height (0.0 for none;
    /// [`crate::burned_text`]); `None` when no keyframe was checked for it.
    /// Absent (empty) in passes stored before it existed.
    #[serde(default)]
    pub bottom_text: Vec<Option<f32>>,
    /// The audio stream analysed (container stream index); `None` when the
    /// file has no audio, in which case every line is [`LineSignals::NONE`].
    pub audio_stream: Option<u32>,
    pub channel: ChannelPick,
    /// Seconds of audio analysed.
    pub analysed_seconds: f32,
    /// Demux read errors skipped (the pass stops after 64 in a row); for the
    /// server to log.
    pub read_errors: u32,
}

/// Direct-form-II-transposed biquad, `f64` state.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(b: [f64; 3], a1: f64, a2: f64) -> Self {
        Biquad {
            b0: b[0],
            b1: b[1],
            b2: b[2],
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Second-order Butterworth-family high-pass or low-pass at `f0` with
    /// quality `q`, via the bilinear transform.
    fn pass(high: bool, f0: f64, q: f64, fs: f64) -> Self {
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let a0 = 1.0 + k / q + k * k;
        let a1 = 2.0 * (k * k - 1.0) / a0;
        let a2 = (1.0 - k / q + k * k) / a0;
        let b = if high {
            [1.0 / a0, -2.0 / a0, 1.0 / a0]
        } else {
            [k * k / a0, 2.0 * k * k / a0, k * k / a0]
        };
        Biquad::new(b, a1, a2)
    }

    /// BS.1770 stage 1: the head-related high shelf.
    fn k_shelf(fs: f64) -> Self {
        let (f0, gain_db, q) = (
            1_681.974_450_955_533,
            3.999_843_853_973_347,
            0.707_175_236_955_419_6,
        );
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let vh = 10f64.powf(gain_db / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / q + k * k;
        Biquad::new(
            [
                (vh + vb * k / q + k * k) / a0,
                2.0 * (k * k - vh) / a0,
                (vh - vb * k / q + k * k) / a0,
            ],
            2.0 * (k * k - 1.0) / a0,
            (1.0 - k / q + k * k) / a0,
        )
    }

    /// BS.1770 stage 2: the revised low-frequency B-curve (a 38 Hz high-pass).
    fn k_rlb(fs: f64) -> Self {
        Biquad::pass(true, 38.135_470_876_024_44, 0.500_327_037_323_877_3, fs)
    }
}

/// Per-block reduction of the K-weighted, high-passed and raw signal.
#[derive(Debug, Clone, Copy)]
struct Block {
    k_sq: f32,
    hp_sq: f32,
    raw_sq: f32,
    peak: f32,
    n: u16,
}

/// Streaming accumulator. Feed mono samples at [`ANALYSIS_RATE`] with
/// [`push`](Self::push), then [`finish`](Self::finish).
pub struct SignalAnalyser {
    windows: Vec<(u64, u64)>,
    shelf: Biquad,
    rlb: Biquad,
    hp: Biquad,
    dec_lp: [Biquad; 2],
    blocks: Vec<Block>,
    cur: Block,
    cur_k: f64,
    cur_hp: f64,
    cur_raw: f64,
    dec: Vec<f32>,
    samples: u64,
}

impl SignalAnalyser {
    /// `windows` are the lines' `[start_ms, end_ms]` on the same timeline as
    /// the first sample pushed (t = 0).
    pub fn new(windows: &[(u64, u64)]) -> Self {
        let fs = f64::from(ANALYSIS_RATE);
        // 4th-order Butterworth at 1 kHz as two cascaded sections.
        let dec_lp = [
            Biquad::pass(false, 1_000.0, 0.541_196_100_146_197, fs),
            Biquad::pass(false, 1_000.0, 1.306_562_964_876_376_4, fs),
        ];
        SignalAnalyser {
            windows: windows.to_vec(),
            shelf: Biquad::k_shelf(fs),
            rlb: Biquad::k_rlb(fs),
            hp: Biquad::pass(true, HIGH_BAND_HZ, std::f64::consts::FRAC_1_SQRT_2, fs),
            dec_lp,
            blocks: Vec::new(),
            cur: Block {
                k_sq: 0.0,
                hp_sq: 0.0,
                raw_sq: 0.0,
                peak: 0.0,
                n: 0,
            },
            cur_k: 0.0,
            cur_hp: 0.0,
            cur_raw: 0.0,
            dec: Vec::new(),
            samples: 0,
        }
    }

    /// Samples consumed so far.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Append `n` samples of silence (a gap in the stream's timestamps).
    pub fn push_silence(&mut self, n: usize) {
        const CHUNK: [f32; 4096] = [0.0; 4096];
        let mut left = n;
        while left > 0 {
            let take = left.min(CHUNK.len());
            self.push(&CHUNK[..take]);
            left -= take;
        }
    }

    pub fn push(&mut self, mono: &[f32]) {
        for &s in mono {
            let x = f64::from(s);
            let k = self.rlb.process(self.shelf.process(x));
            let hp = self.hp.process(x);
            self.cur_k += k * k;
            self.cur_hp += hp * hp;
            self.cur_raw += x * x;
            self.cur.peak = self.cur.peak.max(s.abs());
            self.cur.n += 1;
            let d0 = self.dec_lp[0].process(x);
            let d = self.dec_lp[1].process(d0);
            if self.samples % DECIM as u64 == 0 {
                self.dec.push(d as f32);
            }
            self.samples += 1;
            if usize::from(self.cur.n) == BLOCK {
                self.flush_block();
            }
        }
    }

    fn flush_block(&mut self) {
        let mut b = self.cur;
        b.k_sq = self.cur_k as f32;
        b.hp_sq = self.cur_hp as f32;
        b.raw_sq = self.cur_raw as f32;
        self.blocks.push(b);
        self.cur = Block {
            k_sq: 0.0,
            hp_sq: 0.0,
            raw_sq: 0.0,
            peak: 0.0,
            n: 0,
        };
        self.cur_k = 0.0;
        self.cur_hp = 0.0;
        self.cur_raw = 0.0;
    }

    /// One [`LineSignals`] per window, and the seconds of audio consumed.
    pub fn finish(mut self) -> (Vec<LineSignals>, f32) {
        if self.cur.n > 0 {
            self.flush_block();
        }
        let seconds = (self.samples as f64 / f64::from(ANALYSIS_RATE)) as f32;
        let n_frames = if self.dec.len() >= YIN_FRAME + TAU_MAX {
            (self.dec.len() - YIN_FRAME - TAU_MAX) / YIN_HOP + 1
        } else {
            0
        };
        // 0 = not yet computed, 1 = unvoiced, 2 = voiced (f0 in `f0s`).
        let mut state = vec![0u8; n_frames];
        let mut f0s = vec![0f32; n_frames];
        let lines = self
            .windows
            .iter()
            .map(|&w| self.line(w, &mut state, &mut f0s))
            .collect();
        (lines, seconds)
    }

    fn line(&self, (start, end): (u64, u64), state: &mut [u8], f0s: &mut [f32]) -> LineSignals {
        let (start, end) = if end >= start {
            (start, end)
        } else {
            (end, start)
        };
        let (start, end) = if end - start < MIN_WINDOW_MS {
            let s = ((start + end) / 2).saturating_sub(MIN_WINDOW_MS / 2);
            (s, s + MIN_WINDOW_MS)
        } else {
            (start, end)
        };
        let rate = u64::from(ANALYSIS_RATE);
        let b0 = (start.saturating_mul(rate) / 1000) as usize / BLOCK;
        let b1 =
            (((end.saturating_mul(rate) / 1000) as usize).div_ceil(BLOCK)).min(self.blocks.len());
        if b0 >= b1 {
            return LineSignals::NONE;
        }
        let (mut k, mut hp, mut raw, mut peak, mut n) = (0f64, 0f64, 0f64, 0f32, 0u64);
        for b in &self.blocks[b0..b1] {
            k += f64::from(b.k_sq);
            hp += f64::from(b.hp_sq);
            raw += f64::from(b.raw_sq);
            peak = peak.max(b.peak);
            n += u64::from(b.n);
        }
        let n = n.max(1) as f64;
        let db = |x: f64| -> f32 {
            if x <= 1e-12 {
                FLOOR_DB
            } else {
                (10.0 * x.log10()).max(f64::from(FLOOR_DB)) as f32
            }
        };
        let loudness = db(k / n) - 0.691;
        let peak_db = db(f64::from(peak) * f64::from(peak));
        let share = if raw > 1e-12 {
            (hp / raw).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };

        // Pitch frames whose centre lies in the span.
        let to_frame = |ms: f64| (ms * YIN_RATE / 1000.0 - (YIN_FRAME / 2) as f64) / YIN_HOP as f64;
        let first = to_frame(start as f64).ceil().max(0.0) as usize;
        let last = to_frame(end as f64).floor();
        let mut voiced: Vec<f32> = Vec::new();
        let mut frames = 0usize;
        if last >= 0.0 {
            let last = (last as usize).min(state.len().saturating_sub(1));
            if !state.is_empty() {
                for i in first..=last {
                    if state[i] == 0 {
                        let x = &self.dec[i * YIN_HOP..i * YIN_HOP + YIN_FRAME + TAU_MAX];
                        match yin_f0(x) {
                            Some(f) => {
                                state[i] = 2;
                                f0s[i] = f;
                            }
                            None => state[i] = 1,
                        }
                    }
                    frames += 1;
                    if state[i] == 2 {
                        voiced.push(f0s[i]);
                    }
                }
            }
        }
        let pitch = if voiced.len() >= MIN_VOICED_FRAMES {
            voiced.sort_by(f32::total_cmp);
            let m = voiced.len() / 2;
            Some(if voiced.len() % 2 == 1 {
                voiced[m]
            } else {
                (voiced[m - 1] + voiced[m]) / 2.0
            })
        } else {
            None
        };
        LineSignals {
            loudness_lufs: Some(loudness.max(FLOOR_DB)),
            peak_dbfs: Some(peak_db),
            pitch_hz: pitch,
            voiced_ratio: if frames == 0 {
                0.0
            } else {
                voiced.len() as f32 / frames as f32
            },
            high_band_share: Some(share),
        }
    }
}

/// A gap or overlap in the audio timestamps under this is jitter.
const PLACE_TOLERANCE_MS: i64 = 50;
/// Longest gap filled with silence; a larger one is a bad timestamp.
const PLACE_MAX_GAP_MS: i64 = 3_600_000;

/// Where a decoded audio frame goes on the timeline.
///
/// `have` samples are already in, the frame starts at `frame_ms` and holds
/// `n` samples at [`ANALYSIS_RATE`]; at most `cap` samples are wanted in all.
/// Returns `(silence, skip)`: silence samples to insert first (a gap, capped
/// so `have + silence <= cap`), and leading samples of the frame to drop
/// (before t = 0, as with a negative start pts from MP4 priming or an edit
/// list, or overlapping what is already in). Both are 0 within the 50 ms
/// jitter tolerance.
pub fn place_frame(have: u64, frame_ms: i64, n: usize, cap: u64) -> (usize, usize) {
    let rate = i64::from(ANALYSIS_RATE);
    let have_i = i64::try_from(have).unwrap_or(i64::MAX);
    let at = frame_ms.saturating_mul(rate) / 1000;
    let diff_ms = (at - have_i).saturating_mul(1000) / rate;
    if diff_ms > PLACE_TOLERANCE_MS && diff_ms <= PLACE_MAX_GAP_MS {
        let room = cap.saturating_sub(have);
        let silence = u64::try_from(at - have_i).unwrap_or(0).min(room);
        (silence as usize, 0)
    } else if diff_ms < -PLACE_TOLERANCE_MS {
        let skip = usize::try_from(have_i - at).unwrap_or(usize::MAX);
        (0, skip.min(n))
    } else {
        (0, 0)
    }
}

/// Analyse `pcm` (mono, [`ANALYSIS_RATE`], t = 0 at sample 0) for `windows`.
pub fn analyse_lines(pcm: &[f32], windows: &[(u64, u64)]) -> Vec<LineSignals> {
    let mut a = SignalAnalyser::new(windows);
    a.push(pcm);
    a.finish().0
}

/// YIN over one frame of `YIN_FRAME + TAU_MAX` samples at [`YIN_RATE`].
fn yin_f0(x: &[f32]) -> Option<f32> {
    debug_assert!(x.len() >= YIN_FRAME + TAU_MAX);
    let head = &x[..YIN_FRAME];
    let energy = head.iter().map(|v| v * v).sum::<f32>() / YIN_FRAME as f32;
    if energy < YIN_SILENCE {
        return None;
    }
    // Difference function, then the cumulative-mean normalisation.
    let mut cmnd = [1.0f32; TAU_MAX + 1];
    let mut running = 0.0f32;
    for tau in 1..=TAU_MAX {
        let d: f32 = head
            .iter()
            .zip(&x[tau..tau + YIN_FRAME])
            .map(|(a, b)| (a - b) * (a - b))
            .sum();
        running += d;
        cmnd[tau] = if running > 0.0 {
            d * tau as f32 / running
        } else {
            1.0
        };
    }
    let mut tau = (TAU_MIN..=TAU_MAX).find(|&t| cmnd[t] < YIN_THRESHOLD)?;
    while tau < TAU_MAX && cmnd[tau + 1] < cmnd[tau] {
        tau += 1;
    }
    let mut t = tau as f32;
    if tau > 1 && tau < TAU_MAX {
        let (a, b, c) = (cmnd[tau - 1], cmnd[tau], cmnd[tau + 1]);
        let denom = a - 2.0 * b + c;
        if denom.abs() > 1e-9 {
            t += (0.5 * (a - c) / denom).clamp(-1.0, 1.0);
        }
    }
    let f0 = YIN_RATE / f64::from(t);
    (F0_MIN..=F0_MAX).contains(&f0).then_some(f0 as f32)
}

/// Fraction of inter-keyframe gaps that must sit on the cadence for it to
/// count as a regular GOP.
const CADENCE_SHARE: f64 = 0.3;
/// A keyframe arriving more than this fraction of the cadence early is off it.
const CADENCE_TOLERANCE: f64 = 0.05;

/// The keyframes that are likely scene cuts.
///
/// Encoders place a keyframe at a cut, and otherwise at a fixed maximum GOP
/// length. Reading packet flags is a demux-only proxy for scene detection, at
/// the price of missing cuts in a fixed-GOP encode (ADR-0067).
///
/// 1. The cadence `M` is the median gap between consecutive keyframes.
/// 2. A keyframe is a cut when it arrives early: its gap from the previous
///    keyframe is under `0.95 M`. A keyframe after a cut is a full `M` after
///    it (the encoder's GOP counter restarts at any keyframe), so it counts
///    as regular: the cadence resets rather than the whole list shifting.
///    A late keyframe (gap over `M`) is a dropped one, not a cut. The 5 %
///    margin is wider than timestamp rounding (one frame is 2-4 % of a 1-2 s
///    GOP) and tight enough to catch a cut 0.2 s before a 2 s boundary.
/// 3. If fewer than 30 % of the gaps lie within 5 % of `M` there is no
///    cadence (a scene-adaptive encode, or a keyframe per cut): every
///    keyframe after the first is returned.
///
/// The first keyframe is never a cut (it is the start of the stream), and
/// fewer than three keyframes cannot establish a cadence, so none is.
/// `keyframes_ms` need not be sorted or unique.
pub fn cuts_from_keyframes(keyframes_ms: &[u64]) -> Vec<u64> {
    let mut kf = keyframes_ms.to_vec();
    kf.sort_unstable();
    kf.dedup();
    if kf.len() < 3 {
        return Vec::new();
    }
    let gaps: Vec<u64> = kf.windows(2).map(|w| w[1] - w[0]).collect();
    let mut sorted = gaps.clone();
    sorted.sort_unstable();
    let m = sorted[sorted.len() / 2] as f64;
    let on_cadence = gaps
        .iter()
        .filter(|&&g| ((g as f64) - m).abs() <= CADENCE_TOLERANCE * m)
        .count();
    if (on_cadence as f64) < CADENCE_SHARE * gaps.len() as f64 {
        return kf[1..].to_vec();
    }
    kf.windows(2)
        .filter(|w| ((w[1] - w[0]) as f64) < (1.0 - CADENCE_TOLERANCE) * m)
        .map(|w| w[1])
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    const FS: f32 = ANALYSIS_RATE as f32;

    fn tone(hz: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(secs * FS) as usize)
            .map(|i| amp * (2.0 * PI * hz * i as f32 / FS).sin())
            .collect()
    }

    /// A voice-like buzz: the fundamental and its first harmonics.
    fn voice(f0: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(secs * FS) as usize)
            .map(|i| {
                let t = i as f32 / FS;
                amp * (0.6 * (2.0 * PI * f0 * t).sin()
                    + 0.3 * (2.0 * PI * 2.0 * f0 * t).sin()
                    + 0.1 * (2.0 * PI * 3.0 * f0 * t).sin())
            })
            .collect()
    }

    fn noise(amp: f32, secs: f32, seed: u32) -> Vec<f32> {
        let mut s = seed.max(1);
        (0..(secs * FS) as usize)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                amp * ((s as f32 / u32::MAX as f32) * 2.0 - 1.0)
            })
            .collect()
    }

    fn one(pcm: &[f32], w: (u64, u64)) -> LineSignals {
        analyse_lines(pcm, &[w])[0]
    }

    #[test]
    fn full_scale_997hz_sine_is_minus_3_lufs() {
        // BS.1770 calibration: a 0 dBFS-peak 997 Hz sine reads -3.01 LUFS.
        let l = one(&tone(997.0, 1.0, 3.0), (500, 2500))
            .loudness_lufs
            .unwrap();
        assert!((l - -3.01).abs() < 0.15, "{l}");
    }

    #[test]
    fn loudness_follows_level_in_lu() {
        let loud = tone(440.0, 0.5, 2.0);
        let quiet = tone(440.0, 0.05, 2.0);
        let a = one(&loud, (200, 1800)).loudness_lufs.unwrap();
        let b = one(&quiet, (200, 1800)).loudness_lufs.unwrap();
        assert!((a - b - 20.0).abs() < 0.3, "{a} {b}");
    }

    #[test]
    fn k_weighting_cuts_bass_and_lifts_treble() {
        let at = |hz| one(&tone(hz, 0.5, 2.0), (200, 1800)).loudness_lufs.unwrap();
        // The RLB high-pass takes ~15 dB off 20 Hz; the shelf adds ~4 dB at
        // 5 kHz relative to 1 kHz's ~0.7 dB.
        assert!(at(1000.0) - at(20.0) > 12.0);
        let hi = at(4000.0) - at(1000.0);
        assert!((2.0..4.0).contains(&hi), "{hi}");
    }

    #[test]
    fn peak_is_in_dbfs() {
        let p = one(&tone(300.0, 0.25, 2.0), (200, 1800)).peak_dbfs.unwrap();
        assert!((p - -12.04).abs() < 0.2, "{p}");
    }

    #[test]
    fn short_lines_use_a_400ms_window_centred_on_them() {
        // Tone only in [1.0, 1.4] s; a 100 ms line at its centre must hear it
        // fully, which a 100 ms window would too, but a line at 1.2 s with the
        // tone ending at 1.25 s shows the widening: half the window is silent.
        let mut pcm = vec![0.0; (FS * 3.0) as usize];
        let t = tone(440.0, 0.5, 0.25);
        let at = (FS * 1.0) as usize;
        pcm[at..at + t.len()].copy_from_slice(&t);
        let narrow = one(&pcm, (1100, 1200)).loudness_lufs.unwrap();
        // Window is 1.0-1.4 s: 0.25 s of 0.4 s is tone, so ~2 dB below solid.
        let solid = one(&tone(440.0, 0.5, 3.0), (1100, 1200))
            .loudness_lufs
            .unwrap();
        assert!((solid - narrow - 2.04).abs() < 0.5, "{solid} {narrow}");
    }

    #[test]
    fn silence_is_floored_and_past_the_end_is_none() {
        let pcm = vec![0.0; (FS * 2.0) as usize];
        let l = one(&pcm, (500, 1500));
        assert_eq!(l.loudness_lufs, Some(FLOOR_DB));
        assert_eq!(l.pitch_hz, None);
        assert_eq!(l.voiced_ratio, 0.0);
        assert_eq!(one(&pcm, (5_000, 6_000)), LineSignals::NONE);
    }

    #[test]
    fn pitch_of_a_voiced_buzz() {
        for f0 in [100.0f32, 180.0, 220.0, 330.0, 450.0] {
            let s = one(&voice(f0, 0.4, 2.0), (200, 1800));
            let p = s.pitch_hz.unwrap_or(0.0);
            assert!((p - f0).abs() / f0 < 0.03, "f0 {f0}: got {p}");
            assert!(s.voiced_ratio > 0.9, "f0 {f0}: ratio {}", s.voiced_ratio);
        }
    }

    #[test]
    fn noise_is_unvoiced() {
        let s = one(&noise(0.4, 2.0, 7), (200, 1800));
        assert_eq!(s.pitch_hz, None);
        assert!(s.voiced_ratio < 0.1, "{}", s.voiced_ratio);
    }

    #[test]
    fn silence_is_unvoiced() {
        let s = one(&vec![0.0; (FS * 2.0) as usize], (200, 1800));
        assert_eq!(s.pitch_hz, None);
        assert_eq!(s.voiced_ratio, 0.0);
    }

    #[test]
    fn high_band_share_separates_hiss_from_a_low_tone() {
        let low = one(&tone(200.0, 0.5, 2.0), (200, 1800))
            .high_band_share
            .unwrap();
        let hiss = one(&noise(0.5, 2.0, 3), (200, 1800))
            .high_band_share
            .unwrap();
        let hi = one(&tone(4800.0, 0.5, 2.0), (200, 1800))
            .high_band_share
            .unwrap();
        assert!(low < 0.01, "{low}");
        assert!(hi > 0.8, "{hi}");
        // White noise over 0-5.5 kHz: ~(5.5-3.4)/5.5 = 38 % above the cut-off.
        assert!((0.25..0.5).contains(&hiss), "{hiss}");
    }

    #[test]
    fn lines_pick_their_own_span_and_may_overlap() {
        let mut pcm = voice(200.0, 0.5, 2.0);
        pcm.extend(voice(300.0, 0.05, 2.0));
        let r = analyse_lines(&pcm, &[(200, 1800), (2200, 3800), (1000, 3000)]);
        let (a, b, c) = (
            r[0].loudness_lufs.unwrap(),
            r[1].loudness_lufs.unwrap(),
            r[2].loudness_lufs.unwrap(),
        );
        assert!((a - b - 20.0).abs() < 0.5, "{a} {b}");
        assert!(b < c && c < a, "overlap sits between: {a} {b} {c}");
        assert!((r[0].pitch_hz.unwrap() - 200.0).abs() < 6.0);
        assert!((r[1].pitch_hz.unwrap() - 300.0).abs() < 9.0);
    }

    #[test]
    fn chunked_push_equals_one_push() {
        let pcm = voice(210.0, 0.3, 3.0);
        let w = [(300, 2700)];
        let whole = analyse_lines(&pcm, &w);
        let mut a = SignalAnalyser::new(&w);
        for c in pcm.chunks(1000) {
            a.push(c);
        }
        assert_eq!(a.finish().0, whole);
    }

    /// A 24-minute episode with 500 lines, timed. Informative on an optimised
    /// build (`cargo test --release -p pharos-transcode --lib -- --ignored
    /// --nocapture twenty_four`); the unoptimised dev profile is ~30x slower,
    /// so this is ignored in the default run.
    #[test]
    #[ignore = "timing note; run in release"]
    fn twenty_four_minutes_of_dsp_time() {
        let secs = 24 * 60;
        let pcm = voice(190.0, 0.2, 60.0).repeat(24);
        assert_eq!(pcm.len(), (FS as usize) * secs);
        let windows: Vec<(u64, u64)> = (0..500u64)
            .map(|i| (i * 2_800 + 100, i * 2_800 + 2_600))
            .collect();
        let t = std::time::Instant::now();
        let r = analyse_lines(&pcm, &windows);
        let el = t.elapsed();
        eprintln!("24 min, 500 lines: {el:?}");
        assert_eq!(r.len(), 500);
        assert!(el.as_secs_f32() < 1.0, "{el:?}");
    }

    #[test]
    fn place_frame_fills_gaps_drops_negative_starts_and_caps() {
        let r = i64::from(ANALYSIS_RATE);
        let ms = |m: i64| (m * r / 1000) as usize;
        // On time, or within jitter: untouched.
        assert_eq!(place_frame(0, 0, 1000, u64::MAX), (0, 0));
        assert_eq!(place_frame(0, 30, 1000, u64::MAX), (0, 0));
        // A late start is filled with silence.
        assert_eq!(place_frame(0, 1000, 1000, u64::MAX), (ms(1000), 0));
        // A first pts of -1500 ms drops 1.5 s so the rest lands at t = 0.
        assert_eq!(place_frame(0, -1500, ms(2000), u64::MAX), (0, ms(1500)));
        // A frame wholly before t = 0 is dropped whole.
        assert_eq!(place_frame(0, -3000, ms(100), u64::MAX), (0, ms(100)));
        // Overlap with samples already in is dropped (to within a sample).
        let (silence, skip) = place_frame(ms(1000) as u64, 500, ms(1000), u64::MAX);
        assert_eq!(silence, 0);
        assert!(skip.abs_diff(ms(500)) <= 1, "{skip}");
        // A bogus gap never inserts more than the cap allows, or over an hour.
        assert_eq!(place_frame(0, 600_000, 10, 1000), (1000, 0));
        assert_eq!(place_frame(1000, 600_000, 10, 1000), (0, 0));
        assert_eq!(place_frame(0, 7_200_000, 10, u64::MAX), (0, 0));
    }

    #[test]
    fn a_regular_gop_has_no_cuts() {
        let kf: Vec<u64> = (0..12).map(|i| i * 2000).collect();
        assert!(cuts_from_keyframes(&kf).is_empty());
    }

    #[test]
    fn inserted_keyframes_are_cuts_and_the_cadence_resets() {
        // 2 s GOP; cuts at 5.3 s and 9.1 s restart the counter.
        let kf = [0, 2000, 4000, 5300, 7300, 9100, 11100, 13100];
        assert_eq!(cuts_from_keyframes(&kf), vec![5300, 9100]);
    }

    #[test]
    fn unsorted_duplicated_input_is_normalised() {
        let kf = [4000, 0, 2000, 2000, 5300, 7300, 9300];
        assert_eq!(cuts_from_keyframes(&kf), vec![5300]);
    }

    #[test]
    fn an_irregular_list_has_no_cadence_so_every_keyframe_is_a_cut() {
        let kf = [0, 700, 2900, 3500, 7100, 7800, 10_400, 11_000, 14_900];
        assert_eq!(cuts_from_keyframes(&kf), kf[1..].to_vec());
    }

    #[test]
    fn too_few_keyframes_give_no_cuts() {
        assert!(cuts_from_keyframes(&[]).is_empty());
        assert!(cuts_from_keyframes(&[0]).is_empty());
        assert!(cuts_from_keyframes(&[0, 500]).is_empty());
    }
}
