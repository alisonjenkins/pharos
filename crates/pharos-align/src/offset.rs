//! Whole-song offset between synced lyric stamps and the audio, as
//! `pharos_aligner.offset` finds it.
//!
//! Lyrics timed for another cut of a song are early or late by the same
//! amount on every line, and each line is aligned only inside its own
//! stamped window, so a few distinctive lines are looked for in wide windows
//! first; when they agree on a shift, every line is moved by it.

use std::collections::HashMap;

use crate::plan::{Line, CAP_EXTRA_MS, CAP_MS_PER_WORD};
use crate::text::words;

/// How far either side of its stamp a probe line is looked for: covers a
/// re-recording a few seconds longer or shorter in its intro.
pub const SEARCH_MS: i64 = 8_000;
pub const PROBES: usize = 7;
/// Short lines ("oh", "yeah") match any vocal; three words pin a line down.
pub const MIN_PROBE_WORDS: usize = 3;
pub const MIN_AGREEING: usize = 3;
/// Probes within this of their median count as agreeing on the shift.
pub const AGREE_MS: f64 = 750.0;
/// Smaller shifts are within what the stamps and per-line windows absorb.
pub const NEGLIGIBLE_MS: i64 = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    pub offset_ms: i64,
    /// Each probe's own shift, for the log; `None` where it found nothing.
    pub probes: Vec<Option<i64>>,
    pub reason: String,
}

/// Up to `PROBES` lines spread over the song, long enough and unrepeated.
pub fn probe_indices(lines: &[Line]) -> Vec<usize> {
    let keyed: Vec<Vec<String>> = lines
        .iter()
        .map(|l| words(&l.text).into_iter().map(|w| w.token).collect())
        .collect();
    let mut counts: HashMap<&[String], usize> = HashMap::new();
    for k in &keyed {
        *counts.entry(k.as_slice()).or_default() += 1;
    }
    let candidates: Vec<usize> = keyed
        .iter()
        .enumerate()
        .filter(|(_, k)| k.len() >= MIN_PROBE_WORDS && counts[k.as_slice()] == 1)
        .map(|(i, _)| i)
        .collect();
    if candidates.len() <= PROBES {
        return candidates;
    }
    // Python's float `step`, rounding included, so the same lines are picked.
    let step = candidates.len() as f64 / PROBES as f64;
    (0..PROBES)
        .map(|n| candidates[(n as f64 * step) as usize])
        .collect()
}

/// The `(start_ms, end_ms)` window each probe line is looked for in, in the
/// order [`estimate`] asks its decoder for them, so a caller can decode them
/// all ahead of time (B1414).
pub fn probe_windows(lines: &[Line]) -> Vec<(i64, i64)> {
    probe_indices(lines)
        .into_iter()
        .map(|i| {
            let line = &lines[i];
            let n = i64::try_from(words(&line.text).len()).unwrap_or(i64::MAX);
            let sung_ms =
                (line.end_ms - line.start_ms).min(n.saturating_mul(CAP_MS_PER_WORD) + CAP_EXTRA_MS);
            let start = (line.start_ms - SEARCH_MS).max(0);
            (start, line.start_ms + sung_ms + SEARCH_MS)
        })
        .collect()
}

/// `statistics.median`: the middle value, or the mean of the middle two.
fn median(sorted: &[i64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2] as f64
    } else {
        (sorted[n / 2 - 1] as f64 + sorted[n / 2] as f64) / 2.0
    }
}

/// Finds the shift that moves the stamped lines onto the singing.
///
/// `decode(start_ms, end_ms)` returns the window's samples, empty when the
/// probe is to be skipped, or an error that ends the estimate (and the song).
/// Every probe is decoded first; `align_wide` then gets all the non-empty
/// windows with their tokens at once, since they are independent and can
/// share forward passes, and returns token spans relative to each window,
/// empty where the line was not found, or likewise an error that ends it.
pub fn estimate<D, A, E>(lines: &[Line], mut decode: D, mut align_wide: A) -> Result<Estimate, E>
where
    D: FnMut(i64, i64) -> Result<Vec<f32>, E>,
    A: FnMut(&[(&[f32], &[String])]) -> Result<Vec<Vec<(u64, u64)>>, E>,
{
    let mut windows = Vec::new();
    for (i, (start, end)) in probe_indices(lines).into_iter().zip(probe_windows(lines)) {
        let line = &lines[i];
        let tokens: Vec<String> = words(&line.text).into_iter().map(|w| w.token).collect();
        let pcm = decode(start, end)?;
        windows.push((line.start_ms, start, pcm, tokens));
    }
    let wide: Vec<(&[f32], &[String])> = windows
        .iter()
        .filter(|(_, _, pcm, _)| !pcm.is_empty())
        .map(|(_, _, pcm, tokens)| (pcm.as_slice(), tokens.as_slice()))
        .collect();
    let mut spans = if wide.is_empty() {
        Vec::new()
    } else {
        align_wide(&wide)?
    }
    .into_iter();
    let probes: Vec<Option<i64>> = windows
        .iter()
        .map(|(stamp, start, pcm, _)| {
            let got = if pcm.is_empty() { None } else { spans.next() };
            match got.as_deref().and_then(<[_]>::first) {
                None | Some((0, 0)) => None,
                Some(&(s, _)) => Some(start + i64::try_from(s).unwrap_or(i64::MAX) - stamp),
            }
        })
        .collect();
    let mut found: Vec<i64> = probes.iter().flatten().copied().collect();
    if found.len() < MIN_AGREEING {
        let reason = format!("only {} probes found their line", found.len());
        return Ok(Estimate {
            offset_ms: 0,
            probes,
            reason,
        });
    }
    found.sort_unstable();
    let middle = median(&found);
    let agreeing: Vec<i64> = found
        .iter()
        .copied()
        .filter(|&p| (p as f64 - middle).abs() <= AGREE_MS)
        .collect();
    let agree = format!("{} of {} probes agree", agreeing.len(), found.len());
    if agreeing.len() < MIN_AGREEING.max(found.len() / 2 + 1) {
        return Ok(Estimate {
            offset_ms: 0,
            probes,
            reason: agree,
        });
    }
    // `int()` truncates toward zero; a half from an even count is dropped.
    let offset = median(&agreeing).trunc() as i64;
    if offset.abs() < NEGLIGIBLE_MS {
        let reason = format!("shift {offset} ms is negligible");
        return Ok(Estimate {
            offset_ms: 0,
            probes,
            reason,
        });
    }
    Ok(Estimate {
        offset_ms: offset,
        probes,
        reason: agree,
    })
}

/// Every line moved by `offset_ms`, keeping times at or after zero.
pub fn shift(lines: Vec<Line>, offset_ms: i64) -> Vec<Line> {
    if offset_ms == 0 {
        return lines;
    }
    lines
        .into_iter()
        .map(|l| Line {
            start_ms: (l.start_ms + offset_ms).max(0),
            end_ms: (l.end_ms + offset_ms).max(0),
            text: l.text,
        })
        .collect()
}
