//! FR-009: the coarse search for stage 2. It never calls the aligner: it marks
//! 100 ms frames of the audio as speech by energy and zero-crossing rate, then
//! slides the track's cue on/off pattern over them (per speed ratio) and keeps
//! the offsets where the two line up best.
//!
//! Memory: one byte per frame while listening and one `bool` per frame after
//! (about 27 KB for 45 min); the audio itself is never held.

/// Sample rate the audio is decoded at, mono `s16`.
pub const RATE: u32 = 8_000;
/// One frame of the speech map.
pub const FRAME_MS: i64 = 100;
/// Samples per frame.
const FRAME: usize = (RATE as usize) * (FRAME_MS as usize) / 1000;
/// FR-008: offsets searched either way.
pub const RANGE_MS: i64 = 120_000;
/// FR-009: best offsets kept per ratio.
pub const KEEP: usize = 3;
/// Peaks closer than this are one: the aligner's own search (±8 s) covers both.
pub const PEAK_GAP_MS: i64 = 8_000;
/// FR-008: speed ratios tried besides 1, each `video time / subtitle time`.
pub const RATIOS: [f64; 4] = [25.0 / 23.976, 23.976 / 25.0, 24.0 / 23.976, 23.976 / 24.0];

/// Speech is this many half-decibels above the noise floor (9 dB).
const ABOVE_FLOOR: u8 = 18;
/// The floor is this percentile of the non-silent frames' levels.
const FLOOR_PERCENTILE: usize = 10;
/// More crossings per sample than this is hiss or fricative noise, not voice.
const MAX_ZCR: f64 = 0.35;

/// Folds decoded samples into one level byte per frame.
#[derive(Debug, Default)]
pub struct Listener {
    /// Half-decibels above -100 dBFS, 0 for silence or a noise-like frame.
    levels: Vec<u8>,
    sum_sq: f64,
    crossings: u32,
    n: usize,
    last: i16,
}

impl Listener {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds 8 kHz mono samples, in order, in any chunk size.
    pub fn push(&mut self, samples: &[i16]) {
        for &s in samples {
            self.sum_sq += f64::from(s) * f64::from(s);
            if (s < 0) != (self.last < 0) {
                self.crossings += 1;
            }
            self.last = s;
            self.n += 1;
            if self.n == FRAME {
                self.close_frame();
            }
        }
    }

    fn close_frame(&mut self) {
        let n = self.n as f64;
        let ms = self.sum_sq / n / (32_768.0 * 32_768.0);
        let zcr = f64::from(self.crossings) / n;
        let level = if ms <= 0.0 || zcr > MAX_ZCR {
            0
        } else {
            let dbfs = 10.0 * ms.log10();
            // Clamped to 0..=255 first, so the cast cannot truncate.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let b = ((dbfs + 100.0) * 2.0).clamp(0.0, 255.0) as u8;
            b
        };
        self.levels.push(level);
        self.sum_sq = 0.0;
        self.crossings = 0;
        self.n = 0;
    }

    /// The speech map; a last partial frame is dropped.
    pub fn finish(self) -> Speech {
        let mut voiced: Vec<u8> = self.levels.iter().copied().filter(|&l| l > 0).collect();
        if voiced.is_empty() {
            return Speech::from_frames(vec![false; self.levels.len()]);
        }
        let at = (voiced.len() * FLOOR_PERCENTILE / 100).min(voiced.len() - 1);
        let (_, floor, _) = voiced.select_nth_unstable(at);
        let threshold = floor.saturating_add(ABOVE_FLOOR);
        let loud = |l: u8| l > 0 && l >= threshold;
        if self.levels.iter().any(|&l| loud(l)) {
            return Speech::from_frames(self.levels.iter().map(|&l| loud(l)).collect());
        }
        // Every sound is within the margin of the floor (sound only between
        // digital silences): the sound itself is the best guess at speech.
        Speech::from_frames(self.levels.iter().map(|&l| l > 0).collect())
    }
}

/// Which 100 ms frames of the audio hold speech.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Speech {
    on: Vec<bool>,
}

impl Speech {
    pub fn from_frames(on: Vec<bool>) -> Self {
        Self { on }
    }

    pub fn frames(&self) -> usize {
        self.on.len()
    }

    pub fn speech_frames(&self) -> usize {
        self.on.iter().filter(|&&b| b).count()
    }
}

/// One place the cues could sit: `video_ms = sub_ms * ratio + offset_ms`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shift {
    pub offset_ms: i64,
    pub ratio: f64,
    /// Share of cue frames on speech beyond chance (`-1..=1`).
    pub lift: f64,
}

fn frame_of(ms: f64) -> i64 {
    // Frames of a film fit an i64 many times over.
    #[allow(clippy::cast_possible_truncation)]
    let f = (ms / FRAME_MS as f64).floor() as i64;
    f
}

/// The cues' frames at `ratio`, as merged `[start, end)` runs.
fn cue_runs(cues: &[(i64, i64)], ratio: f64) -> Vec<(i64, i64)> {
    let mut runs: Vec<(i64, i64)> = cues
        .iter()
        .filter(|(a, b)| b > a)
        .map(|&(a, b)| {
            let s = frame_of(a as f64 * ratio);
            (s, frame_of(b as f64 * ratio).max(s + 1))
        })
        .collect();
    runs.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(runs.len());
    for (a, b) in runs {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

/// FR-009: the [`KEEP`] best offsets within ±[`RANGE_MS`] at `ratio`, best
/// first, at least [`PEAK_GAP_MS`] apart, each lining up better than chance.
pub fn shifts(speech: &Speech, cues: &[(i64, i64)], ratio: f64) -> Vec<Shift> {
    shifts_beyond(speech, cues, ratio, None)
}

/// [`shifts`], leaving out offsets within `skip_ms` of 0 (searched already).
/// A peak left out still holds its [`PEAK_GAP_MS`], so its shoulders do not
/// take the slots, but it does not count towards [`KEEP`].
pub fn shifts_beyond(
    speech: &Speech,
    cues: &[(i64, i64)],
    ratio: f64,
    skip_ms: Option<i64>,
) -> Vec<Shift> {
    let n = speech.on.len();
    let runs = cue_runs(cues, ratio);
    let cue_total: i64 = runs.iter().map(|(a, b)| b - a).sum();
    if n == 0 || cue_total == 0 {
        return Vec::new();
    }
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0u32);
    for (i, &on) in speech.on.iter().enumerate() {
        prefix.push(prefix[i] + u32::from(on));
    }
    let len = i64::try_from(n).unwrap_or(i64::MAX);
    let p = f64::from(prefix[n]) / n as f64;
    let at = |f: i64| prefix[usize::try_from(f.clamp(0, len)).unwrap_or(0)];
    let range = RANGE_MS / FRAME_MS;
    let mut scored: Vec<(i64, f64)> = (-range..=range)
        .map(|o| {
            let (mut hits, mut inside) = (0i64, 0i64);
            for &(a, b) in &runs {
                hits += i64::from(at(b + o)) - i64::from(at(a + o));
                inside += (b + o).clamp(0, len) - (a + o).clamp(0, len);
            }
            (o, (hits as f64 - inside as f64 * p) / cue_total as f64)
        })
        .collect();
    scored.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.abs().cmp(&y.0.abs())));
    let gap = PEAK_GAP_MS / FRAME_MS;
    let mut peaks: Vec<Shift> = Vec::new();
    let mut kept = 0;
    for (o, lift) in scored {
        if kept == KEEP || lift <= 0.0 {
            break;
        }
        let o_ms = o * FRAME_MS;
        if peaks
            .iter()
            .all(|k| (k.offset_ms - o_ms).abs() >= gap * FRAME_MS)
        {
            peaks.push(Shift {
                offset_ms: o_ms,
                ratio,
                lift,
            });
            if skip_ms.map_or(true, |skip| o_ms.abs() > skip) {
                kept += 1;
            }
        }
    }
    peaks.retain(|p| skip_ms.map_or(true, |skip| p.offset_ms.abs() > skip));
    peaks
}

/// FR-008: the stage-2 candidates to try, best first, at most `max`: the
/// [`KEEP`] best offsets at ratio 1 and the best one at each of [`RATIOS`].
/// When stage 1 searched (`stage1_ms` is its reach), a ratio-1 offset within
/// it is left out; `None` when stage 1 sent nothing, so every offset is open.
pub fn candidates(
    speech: &Speech,
    cues: &[(i64, i64)],
    stage1_ms: Option<i64>,
    max: usize,
) -> Vec<Shift> {
    let mut all: Vec<Shift> = shifts_beyond(speech, cues, 1.0, stage1_ms);
    for r in RATIOS {
        all.extend(shifts(speech, cues, r).into_iter().take(1));
    }
    all.sort_by(|a, b| b.lift.total_cmp(&a.lift));
    all.truncate(max);
    all
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Speech where `cues` (in ms) say, shifted by `offset_ms` and scaled.
    fn speech_of(cues: &[(i64, i64)], ratio: f64, offset_ms: i64, len_ms: i64) -> Speech {
        let mut on = vec![false; usize::try_from(len_ms / FRAME_MS).unwrap()];
        for &(a, b) in cues {
            let s = frame_of(a as f64 * ratio + offset_ms as f64);
            let e = frame_of(b as f64 * ratio + offset_ms as f64);
            for f in s.max(0)..e.min(i64::try_from(on.len()).unwrap()) {
                on[usize::try_from(f).unwrap()] = true;
            }
        }
        Speech::from_frames(on)
    }

    /// Irregular cues over 40 minutes, like dialogue.
    fn cues() -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        let mut t = 5_000i64;
        let mut seed = 7u64;
        while t < 40 * 60_000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let len = 800 + i64::try_from(seed >> 52).unwrap() % 3_000;
            let gap = 300 + i64::try_from((seed >> 40) & 0xfff).unwrap() * 3;
            out.push((t, t + len));
            t += len + gap;
        }
        out
    }

    #[test]
    fn finds_an_offset_far_outside_the_aligners_reach() {
        let c = cues();
        for want in [20_000, -45_000, 110_000] {
            let s = speech_of(&c, 1.0, want, 45 * 60_000);
            let best = shifts(&s, &c, 1.0);
            assert_eq!(best[0].offset_ms, want, "{best:?}");
            assert!(best[0].lift > 0.5, "{best:?}");
        }
    }

    #[test]
    fn finds_the_speed_ratio_and_its_offset() {
        let c = cues();
        let r = 25.0 / 23.976;
        let s = speech_of(&c, r, 1_500, 45 * 60_000);
        let got = candidates(&s, &c, Some(8_000), 3);
        assert!((got[0].ratio - r).abs() < 1e-9, "{got:?}");
        assert!((got[0].offset_ms - 1_500).abs() <= FRAME_MS, "{got:?}");
    }

    #[test]
    fn stage_ones_own_range_is_not_tried_again() {
        let c = cues();
        let s = speech_of(&c, 1.0, 3_000, 45 * 60_000);
        let got = candidates(&s, &c, Some(8_000), 8);
        assert!(
            got.iter()
                .all(|g| g.ratio != 1.0 || g.offset_ms.abs() > 8_000),
            "{got:?}"
        );
    }

    #[test]
    fn with_nothing_searched_in_stage_one_a_near_offset_is_open() {
        let c = cues();
        let s = speech_of(&c, 1.0, -5_000, 45 * 60_000);
        let got = candidates(&s, &c, None, 3);
        assert_eq!(got[0].offset_ms, -5_000, "{got:?}");
        assert!(got[0].ratio == 1.0);
    }

    #[test]
    fn peaks_stage_one_searched_do_not_take_the_slots() {
        let c: Vec<(i64, i64)> = (0..12)
            .map(|i| (130_000 + i * 60_000, 131_000 + i * 60_000))
            .collect();
        let mut on = vec![false; 15_000];
        for shift in [-8_000i64, 0, 8_000, 20_000] {
            for &(a, b) in &c {
                for f in (a + shift) / FRAME_MS..(b + shift) / FRAME_MS {
                    on[usize::try_from(f).unwrap()] = true;
                }
            }
        }
        let s = Speech::from_frames(on);
        let got = shifts_beyond(&s, &c, 1.0, Some(8_000));
        assert_eq!(got.first().map(|g| g.offset_ms), Some(20_000), "{got:?}");
        assert!(got.iter().all(|g| g.offset_ms.abs() > 8_000), "{got:?}");
    }

    #[test]
    fn peaks_are_apart_and_silence_finds_nothing() {
        let c = cues();
        let s = speech_of(&c, 1.0, 0, 45 * 60_000);
        let best = shifts(&s, &c, 1.0);
        assert!(best.len() <= KEEP);
        for (i, a) in best.iter().enumerate() {
            for b in &best[i + 1..] {
                assert!((a.offset_ms - b.offset_ms).abs() >= PEAK_GAP_MS);
            }
        }
        let quiet = Speech::from_frames(vec![false; 27_000]);
        assert!(shifts(&quiet, &c, 1.0).is_empty());
        assert!(shifts(&s, &[], 1.0).is_empty());
    }

    #[test]
    fn sound_between_digital_silences_is_speech_even_at_one_level() {
        let mut l = Listener::new();
        let voice: Vec<i16> = (0..FRAME * 3)
            .map(|i| if (i / 20) % 2 == 0 { 3_000 } else { -3_000 })
            .collect();
        for _ in 0..4 {
            l.push(&vec![0; FRAME * 2]);
            l.push(&voice);
        }
        let s = l.finish();
        let want: Vec<bool> = (0..20).map(|f| f % 5 >= 2).collect();
        assert_eq!(s, Speech::from_frames(want));
    }

    #[test]
    fn the_listener_marks_loud_voiced_frames_and_not_hiss_or_quiet() {
        let mut l = Listener::new();
        let tone = |hz: f64, amp: f64, frames: usize| -> Vec<i16> {
            (0..FRAME * frames)
                .map(|i| {
                    let v =
                        amp * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(RATE)).sin();
                    #[allow(clippy::cast_possible_truncation)]
                    let s = v as i16;
                    s
                })
                .collect()
        };
        l.push(&tone(200.0, 100.0, 20)); // quiet room tone
        l.push(&tone(200.0, 8_000.0, 5)); // a voice
                                          // Alternating samples: one crossing per sample, hiss-like.
        let hiss: Vec<i16> = (0..FRAME * 5)
            .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
            .collect();
        l.push(&hiss);
        l.push(&tone(200.0, 100.0, 5));
        l.push(&[1, 2, 3]); // a partial frame is dropped
        let s = l.finish();
        assert_eq!(s.frames(), 35);
        let want: Vec<bool> = (0..35).map(|f| (20..25).contains(&f)).collect();
        assert_eq!(s, Speech::from_frames(want));
    }
}
