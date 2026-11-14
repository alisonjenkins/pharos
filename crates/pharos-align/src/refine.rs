//! Word times from the recogniser's anchors (spec 042, plan steps 5 and 6).
//!
//! The lines from the first anchored line to the last are aligned together,
//! letter by letter, in one pass over the vocals' emission: each anchored
//! line inside a band around its anchor, each line between two anchors
//! inside the gap between them, and every line pulled towards where its
//! first letter is expected. Aligning them together keeps them in order
//! and stops one line borrowing the next line's singing. Lines before the
//! first anchor or after the last keep today's answer.

use crate::anchor::Matched;
use crate::ctc::Emission;
use crate::placement::{Fallback, Method, Placement};
use crate::plan::{self, Line, Stamp};
use crate::service::LineWords;
use crate::text::{self, is_py_space};
use crate::tokens::{tokenize, BLANK, STAR};

/// Audio either side of an anchor the letters may move into.
pub const PAD_MS: i64 = 1_500;
/// The recogniser often ends a line's last word early; its band runs at
/// least this far past where that word starts.
pub const LAST_WORD_MS: i64 = 400;
/// Below this share of lines anchored the song keeps today's answer whole:
/// the prototype's very repetitive songs (chants, "na na") fell under 40%
/// and their few anchors were the least trustworthy.
pub const MIN_ANCHOR_SHARE: f64 = 0.4;
/// Log-probability a line pays per second its first letter lands away from
/// where it is expected; the prototype's setting, enough to stop a line
/// sliding onto a repeat of itself within its band.
pub const ENTRY_COST_PER_S: f64 = 0.3;
/// An anchor whose offset from its line's requested time differs this much
/// from the offsets of the anchors either side, and leaps past a line,
/// is a repeat of the line heard in the wrong place; in the check set the
/// true anchors that leap stay within 3 s of their neighbours, the false
/// ones were 11 s and more away.
pub const OUTLIER_MS: i64 = 5_000;

/// The whole song's vocals emission and the samples it came from.
#[derive(Debug, Clone, Copy)]
pub struct Song<'a> {
    pub emission: Emission<'a>,
    pub n_samples: usize,
}

impl Song<'_> {
    fn ms_per_frame(&self) -> f64 {
        self.n_samples as f64 / self.emission.frames() as f64 / crate::line::SAMPLE_RATE * 1000.0
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn ms(&self, frame: usize) -> u64 {
        (frame as f64 * self.ms_per_frame()) as u64
    }
}

/// Every line's words and how they were placed; `methods` is `None` for a line with no words.
#[derive(Debug, Clone, PartialEq)]
pub struct Refined {
    pub lines: Vec<LineWords>,
    pub methods: Vec<Option<Method>>,
    pub placement: Placement,
}

/// Char offsets of each whitespace-separated word, as `text::words` counts them.
fn split_offsets(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut prev_space = true;
    for (at, c) in text.chars().enumerate() {
        let space = is_py_space(c);
        if !space && prev_space {
            out.push(at);
        }
        prev_space = space;
    }
    out
}

/// The recogniser's own times for an anchored line, per alignable word.
fn anchor_words(line: &Line, words: &[text::Word], starts_ms: &[i64], end: i64) -> LineWords {
    let offsets = split_offsets(&line.text);
    let mut out: Vec<Stamp> = Vec::with_capacity(words.len() + 1);
    let mut last = i64::MIN;
    for w in words {
        let k = offsets.iter().position(|&o| o == w.at).unwrap_or_default();
        let t = starts_ms.get(k).copied().unwrap_or(last).max(last);
        out.push(Stamp {
            at: w.at,
            start_ms: t,
        });
        last = t;
    }
    out.push(Stamp {
        at: line.text.chars().count(),
        start_ms: end.max(last),
    });
    LineWords {
        words: out,
        score: None,
    }
}

/// Anchored lines whose offset stands apart from both neighbouring anchors
/// (or from its one neighbour, at either end of the song) and that would
/// leap past a line with words: one of the lines between it and a
/// neighbour, moved by that neighbour's offset, falls on its far side. An
/// offset far off with no such line is the request's timing being off,
/// not a repeat.
fn outliers(lines: &[Line], matched: &Matched) -> Vec<bool> {
    let offsets: Vec<(usize, i64)> = matched
        .lines
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let a = m.anchor.as_ref()?;
            Some((
                i,
                a.starts_ms.first().copied().unwrap_or(a.end_ms) - lines[i].start_ms,
            ))
        })
        .collect();
    let sung = |j: usize| !text::words(&lines[j].text).is_empty();
    let mut out = vec![false; lines.len()];
    for (k, &(i, off)) in offsets.iter().enumerate() {
        let before = k.checked_sub(1).and_then(|k| offsets.get(k)).copied();
        let after = offsets.get(k + 1).copied();
        let apart = |j: Option<(usize, i64)>| j.map(|(_, o)| (off - o).abs() > OUTLIER_MS);
        let far = match (apart(before), apart(after)) {
            (Some(p), Some(n)) => p && n,
            (Some(x), None) | (None, Some(x)) => x,
            (None, None) => false,
        };
        if !far {
            continue;
        }
        let at = lines[i].start_ms + off;
        let near = |j: Option<(usize, i64)>| j.or(before).or(after).map(|(_, o)| o);
        let leaps_back = near(before).is_some_and(|o| {
            (before.map_or(0, |(p, _)| p + 1)..i).any(|j| sung(j) && lines[j].start_ms + o > at)
        });
        let leaps_on = near(after).is_some_and(|o| {
            (i + 1..after.map_or(lines.len(), |(n, _)| n))
                .any(|j| sung(j) && lines[j].start_ms + o < at)
        });
        out[i] = leaps_back || leaps_on;
    }
    out
}

/// One line's part in the joint alignment: its words' label ids, the band
/// of the song its letters must fall in, and where its first letter is expected.
#[derive(Debug, Clone)]
struct Part {
    words: Vec<Vec<u32>>,
    lo_ms: f64,
    hi_ms: f64,
    expect_ms: f64,
}

impl Part {
    /// Frames the CTC path needs at the least: a frame per letter, plus a
    /// blank between two of the same letter.
    fn min_frames(&self) -> usize {
        let letters: Vec<u32> = self.words.iter().flatten().copied().collect();
        letters.len() + letters.windows(2).filter(|w| w[0] == w[1]).count()
    }
}

/// One aligned part: each word's first and past-the-last frame, and the
/// letters' mean probability.
#[derive(Debug, Clone, PartialEq)]
struct Placed {
    words: Vec<(usize, usize)>,
    score: f64,
}

/// CTC Viterbi over the whole emission for `* part0 * part1 * ... *`, each
/// part's letters held inside its band and its first letter charged
/// `ENTRY_COST_PER_S` for each second it starts away from `expect_ms`.
/// `None` when no path keeps every part in its band.
#[allow(clippy::cast_precision_loss)]
fn joint(song: &Song<'_>, parts: &[Part]) -> Option<Vec<Placed>> {
    const NEG: f64 = -1e30;
    let em = &song.emission;
    let frames = em.frames();
    if frames == 0 {
        return None;
    }
    let fms = song.ms_per_frame();

    // Targets with their owning part; stars between parts belong to none.
    let mut tgt: Vec<u32> = vec![STAR];
    let mut own: Vec<Option<usize>> = vec![None];
    let mut first: Vec<bool> = vec![false];
    for (k, p) in parts.iter().enumerate() {
        for (wi, w) in p.words.iter().enumerate() {
            for (ci, &c) in w.iter().enumerate() {
                tgt.push(c);
                own.push(Some(k));
                first.push(wi == 0 && ci == 0);
            }
        }
        tgt.push(STAR);
        own.push(None);
        first.push(false);
    }
    let n = 2 * tgt.len() + 1;
    let lab = |s: usize| if s % 2 == 1 { tgt[s / 2] } else { BLANK };
    // A blank between two letters of one part is held to that part's band.
    let owner = |s: usize| -> Option<usize> {
        let letter = s % 2 == 1;
        let inside =
            s >= 2 && s + 1 < n && own[s / 2 - 1].is_some() && own[s / 2 - 1] == own[s / 2];
        if letter || inside {
            own[s / 2]
        } else {
            None
        }
    };
    let band: Vec<(f64, f64)> = (0..n)
        .map(|s| {
            owner(s).map_or((f64::MIN, f64::MAX), |k| {
                (parts[k].lo_ms / fms, parts[k].hi_ms / fms)
            })
        })
        .collect();
    // Every target is visited in order, so a state cannot be reached before
    // the latest band start behind it nor after the earliest band end ahead.
    let mut reach_lo = vec![0usize; n];
    let mut lo = 0.0f64;
    for s in 0..n {
        lo = lo.max(band[s].0);
        reach_lo[s] = if lo <= 0.0 { 0 } else { lo.ceil() as usize };
    }
    let mut reach_hi = vec![frames - 1; n];
    let mut hi = f64::MAX;
    for s in (0..n).rev() {
        hi = hi.min(band[s].1);
        reach_hi[s] = if hi >= (frames - 1) as f64 {
            frames - 1
        } else if hi < 0.0 {
            return None;
        } else {
            hi.floor() as usize
        };
    }
    if (0..n).any(|s| reach_lo[s] > reach_hi[s]) {
        return None;
    }
    let skip: Vec<bool> = (0..n)
        .map(|s| s >= 3 && s % 2 == 1 && tgt[s / 2] != tgt[s / 2 - 1])
        .collect();

    // Back-pointers only for the states each frame can reach.
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(frames);
    let mut back: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = Vec::with_capacity(frames);
    let mut a = vec![NEG; n];
    let mut next = vec![NEG; n];
    let (mut s_lo, mut s_hi) = (0usize, 0usize);
    for t in 0..frames {
        while s_lo < n && reach_hi[s_lo] < t {
            s_lo += 1;
        }
        while s_hi < n && reach_lo[s_hi] <= t {
            s_hi += 1;
        }
        ranges.push((s_lo, s_hi));
        offsets.push(back.len());
        let tf = t as f64;
        for s in s_lo..s_hi {
            let mut best = if t == 0 {
                if s < 2 {
                    0.0
                } else {
                    NEG
                }
            } else {
                a[s]
            };
            let mut ptr = 0u8;
            if t > 0 {
                let entry = if s % 2 == 1 && first[s / 2] {
                    let k = own[s / 2].unwrap_or_default();
                    -ENTRY_COST_PER_S * (tf * fms - parts[k].expect_ms).abs() / 1000.0
                } else {
                    0.0
                };
                if s >= 1 && a[s - 1] + entry > best {
                    best = a[s - 1] + entry;
                    ptr = 1;
                }
                if skip[s] && a[s - 2] + entry > best {
                    best = a[s - 2] + entry;
                    ptr = 2;
                }
            }
            let inside = tf >= band[s].0 && tf <= band[s].1;
            next[s] = if inside && best > NEG / 2.0 {
                best + f64::from(em.at(t, lab(s)))
            } else {
                NEG
            };
            back.push(ptr);
        }
        if t > 0 {
            let (p_lo, p_hi) = ranges[t - 1];
            a[p_lo..p_hi.min(s_lo)].fill(NEG);
        }
        a[s_lo..s_hi].copy_from_slice(&next[s_lo..s_hi]);
    }
    let mut s = if a[n - 1] > a[n - 2] { n - 1 } else { n - 2 };
    if a[s] <= NEG / 2.0 {
        return None;
    }
    let mut path = vec![0usize; frames];
    for t in (0..frames).rev() {
        path[t] = s;
        if t > 0 {
            let (r_lo, _) = ranges[t];
            s -= usize::from(back[offsets[t] + s - r_lo]);
        }
    }

    // First and past-the-last frame of each target, and its frames' probability.
    let mut span: Vec<Option<(usize, usize, f64)>> = vec![None; tgt.len()];
    for (t, &st) in path.iter().enumerate() {
        if st % 2 == 1 {
            let p = f64::from(em.at(t, lab(st))).exp();
            let e = span[st / 2].get_or_insert((t, t, 0.0));
            e.1 = t + 1;
            e.2 += p;
        }
    }
    let mut out = Vec::with_capacity(parts.len());
    let mut j = 1;
    for p in parts {
        let mut words = Vec::with_capacity(p.words.len());
        let mut scores = Vec::new();
        for w in &p.words {
            let (s0, _, _) = span[j]?;
            let (_, e1, _) = span[j + w.len() - 1]?;
            for letter in &span[j..j + w.len()] {
                let (a0, a1, sum) = (*letter)?;
                scores.push(sum / (a1 - a0) as f64);
            }
            words.push((s0, e1));
            j += w.len();
        }
        j += 1;
        out.push(Placed {
            words,
            score: scores.iter().sum::<f64>() / scores.len().max(1) as f64,
        });
    }
    Some(out)
}

type Band = (f64, f64, f64);

/// Line `i`, just outside the anchored span, when its answer today runs
/// past the span's placed edge at `edge_ms`: left as it is it would overlap
/// the line inside (V1373). `shift` is the edge anchor's offset.
fn edge_part(
    lines: &[Line],
    today: &[LineWords],
    tokens: &[Option<Vec<Vec<u32>>>],
    i: usize,
    shift: f64,
    edge_ms: f64,
    before: bool,
) -> Option<Part> {
    let words = tokens[i].as_ref()?;
    let first = today[i].words.first()?.start_ms as f64;
    let end = today[i].words.last()?.start_ms as f64;
    let pad = 2.0 * PAD_MS as f64;
    let expect_ms = lines[i].start_ms as f64 + shift;
    if before {
        (end > edge_ms).then(|| Part {
            words: words.clone(),
            lo_ms: first.min(expect_ms) - pad,
            hi_ms: edge_ms + pad,
            expect_ms,
        })
    } else {
        let length = (lines[i].end_ms - lines[i].start_ms) as f64;
        (first < edge_ms).then(|| Part {
            words: words.clone(),
            lo_ms: edge_ms - pad,
            hi_ms: end.max(expect_ms + length) + pad,
            expect_ms,
        })
    }
}

/// Places every line: `today` is today's per-line answer, `matched` the
/// recogniser's anchors.
pub fn place(lines: &[Line], today: &[LineWords], matched: &Matched, song: &Song<'_>) -> Refined {
    let words: Vec<Vec<text::Word>> = lines.iter().map(|l| text::words(&l.text)).collect();
    let with_words = words.iter().filter(|w| !w.is_empty()).count();
    let dropped = outliers(lines, matched);
    let n_dropped = dropped.iter().filter(|&&d| d).count();
    let anchors = matched
        .lines
        .iter()
        .zip(&dropped)
        .filter(|(m, &d)| m.anchor.is_some() && !d)
        .count();
    let share = matched
        .share()
        .map_or(0.0, |_| anchors as f64 / with_words.max(1) as f64);
    let mut out: Vec<LineWords> = today.to_vec();
    let mut methods: Vec<Option<Method>> = words
        .iter()
        .map(|w| (!w.is_empty()).then_some(Method::Kept))
        .collect();
    if share < MIN_ANCHOR_SHARE {
        let mut placement = Placement::kept(with_words, Fallback::LowAnchorShare);
        placement.anchor_share = Some(share);
        placement.dropped_anchors = n_dropped;
        return Refined {
            lines: out,
            methods,
            placement,
        };
    }

    let tokens: Vec<Option<Vec<Vec<u32>>>> = words
        .iter()
        .map(|w| {
            let t: Vec<&str> = w.iter().map(|w| w.token.as_str()).collect();
            (!t.is_empty()).then(|| tokenize(&t).ok()).flatten()
        })
        .collect();
    let frames_ms = song.ms(song.emission.frames()) as f64;
    // Anchored lines with the band around their anchor; a band the song
    // cannot hold keeps the recogniser's times.
    let mut bands: Vec<Option<Band>> = vec![None; lines.len()];
    for (i, m) in matched.lines.iter().enumerate() {
        let (Some(a), Some(t), false) = (m.anchor.as_ref(), tokens[i].as_ref(), dropped[i]) else {
            continue;
        };
        let first = a.starts_ms.first().copied().unwrap_or(a.end_ms);
        let last = a.starts_ms.last().copied().unwrap_or(a.end_ms);
        let lo = (first - PAD_MS) as f64;
        let hi = (a.end_ms.max(last + LAST_WORD_MS) + PAD_MS) as f64;
        let part = Part {
            words: t.clone(),
            lo_ms: lo,
            hi_ms: hi,
            expect_ms: first as f64,
        };
        let room = (hi.min(frames_ms) - lo.max(0.0)) / song.ms_per_frame();
        if room >= part.min_frames() as f64 {
            bands[i] = Some((lo, hi, first as f64));
        } else {
            out[i] = anchor_words(&lines[i], &words[i], &a.starts_ms, a.end_ms);
            methods[i] = Some(Method::Anchored);
        }
    }
    let anchored: Vec<usize> = (0..lines.len()).filter(|&i| bands[i].is_some()).collect();
    if let (Some(&from), Some(&to)) = (anchored.first(), anchored.last()) {
        let mut parts: Vec<(usize, Part)> = Vec::new();
        let (mut prev, mut shift) = (from, 0.0);
        for i in from..=to {
            let Some(t) = tokens[i].clone() else { continue };
            if let Some((lo, hi, expect)) = bands[i] {
                parts.push((
                    i,
                    Part {
                        words: t,
                        lo_ms: lo,
                        hi_ms: hi,
                        expect_ms: expect,
                    },
                ));
                prev = i;
                shift = expect - lines[i].start_ms as f64;
            } else if methods[i] != Some(Method::Anchored) {
                let next = anchored.iter().find(|&&q| q > i).copied().unwrap_or(to);
                let (Some(p), Some(q)) = (bands[prev], bands[next]) else {
                    continue;
                };
                parts.push((
                    i,
                    Part {
                        words: t,
                        lo_ms: p.1 - 2.0 * PAD_MS as f64,
                        hi_ms: q.0 + 2.0 * PAD_MS as f64,
                        expect_ms: lines[i].start_ms as f64 + shift,
                    },
                ));
            }
        }
        let shift_at = |i: usize| bands[i].map_or(0.0, |b| b.2 - lines[i].start_ms as f64);
        let mut before: Vec<(usize, Part)> = Vec::new();
        let mut after: Vec<(usize, Part)> = Vec::new();
        // Each pass that finds an edge line overlapping takes it in and
        // aligns again, so a line moves only when the placed edge reaches it.
        // A line taken in that makes the alignment impossible is left out.
        let mut last = None;
        let result = loop {
            let all: Vec<(usize, Part)> = before
                .iter()
                .rev()
                .chain(&parts)
                .chain(&after)
                .cloned()
                .collect();
            let only: Vec<Part> = all.iter().map(|(_, p)| p.clone()).collect();
            let Some(placed) = joint(song, &only) else {
                break last;
            };
            let (lo_i, hi_i) = (all[0].0, all[all.len() - 1].0);
            let start = placed.first().and_then(|p| p.words.first());
            let end = placed.last().and_then(|p| p.words.last());
            let mut grew = false;
            if let (Some(&(s, _)), Some(i)) = (start, lo_i.checked_sub(1)) {
                let edge = song.ms(s) as f64;
                if let Some(p) = edge_part(lines, today, &tokens, i, shift_at(from), edge, true) {
                    before.push((i, p));
                    grew = true;
                }
            }
            if let (Some(&(_, e)), true) = (end, hi_i + 1 < lines.len()) {
                let edge = song.ms(e) as f64;
                let i = hi_i + 1;
                if let Some(p) = edge_part(lines, today, &tokens, i, shift_at(to), edge, false) {
                    after.push((i, p));
                    grew = true;
                }
            }
            if !grew {
                break Some((all, placed));
            }
            last = Some((all, placed));
        };
        match result {
            Some((parts, placed)) => {
                for ((i, _), p) in parts.iter().zip(placed) {
                    let times: Vec<(u64, u64)> = p
                        .words
                        .iter()
                        .map(|&(a, b)| (song.ms(a), song.ms(b)))
                        .collect();
                    let stamps = plan::stamps(&lines[*i].text, &words[*i], &times, 0);
                    if stamps.is_empty() {
                        continue;
                    }
                    out[*i] = LineWords {
                        words: stamps,
                        score: crate::service::round_score(p.score),
                    };
                    methods[*i] = Some(if bands[*i].is_some() {
                        Method::Refined
                    } else {
                        Method::Between
                    });
                }
            }
            None => {
                for &i in &anchored {
                    if let Some(a) = matched.lines[i].anchor.as_ref() {
                        out[i] = anchor_words(&lines[i], &words[i], &a.starts_ms, a.end_ms);
                        methods[i] = Some(Method::Anchored);
                    }
                }
            }
        }
    }

    let mut placement = Placement::from_methods(methods.iter().flatten().copied(), share);
    placement.dropped_anchors = n_dropped;
    Refined {
        lines: out,
        methods,
        placement,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::anchor::{match_lines, Options};
    use crate::tokens::{label, CLASSES, STAR};
    use crate::transcript::Word as Heard;

    /// 20 ms frames, as MMS_FA gives at 16 kHz.
    const SAMPLES_PER_FRAME: usize = 320;
    const FRAME_MS: i64 = 20;

    /// Mostly blank; each `(letter, ms)` is sung for 60 ms from `ms`; the
    /// star column is 0, as the bundle appends it.
    fn emission(frames: usize, sung: &[(char, i64)]) -> Vec<f32> {
        let mut e = vec![-12.0f32; frames * CLASSES];
        for t in 0..frames {
            e[t * CLASSES] = -0.05;
            e[t * CLASSES + STAR as usize] = 0.0;
        }
        for &(c, ms) in sung {
            let c = label(c).unwrap() as usize;
            let f = usize::try_from(ms / FRAME_MS).unwrap();
            for t in f..(f + 3).min(frames) {
                e[t * CLASSES] = -6.0;
                e[t * CLASSES + c] = -0.01;
            }
        }
        e
    }

    /// The letters of `text` sung from `start_ms`, 100 ms apart, words 200 ms apart.
    fn sing(text: &str, start_ms: i64) -> Vec<(char, i64)> {
        let mut out = Vec::new();
        let mut t = start_ms;
        for w in text.split_whitespace() {
            for c in w.chars() {
                out.push((c, t));
                t += 100;
            }
            t += 100;
        }
        out
    }

    fn line(start_ms: i64, end_ms: i64, text: &str) -> Line {
        Line {
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    fn heard(text: &str, start_s: f64) -> Vec<Heard> {
        text.split_whitespace()
            .enumerate()
            .map(|(k, w)| Heard {
                text: format!(" {w}"),
                start_s: start_s + 0.5 * k as f64,
                end_s: start_s + 0.5 * k as f64 + 0.4,
                prob: 0.9,
            })
            .collect()
    }

    /// Today's answer for a line: its words 300 ms apart from `start_ms`.
    fn today(l: &Line, start_ms: i64) -> LineWords {
        let ws = text::words(&l.text);
        let mut words: Vec<Stamp> = ws
            .iter()
            .enumerate()
            .map(|(k, w)| Stamp {
                at: w.at,
                start_ms: start_ms + 300 * k as i64,
            })
            .collect();
        words.push(Stamp {
            at: l.text.chars().count(),
            start_ms: start_ms + 300 * ws.len() as i64,
        });
        LineWords {
            words,
            score: Some(0.5),
        }
    }

    /// Each word starts on its first letter's 60 ms; the star absorbs a
    /// letter's frames when they score the same, so any of them will do.
    fn sung_at(w: &LineWords, sung: &[i64]) {
        let got = starts(w);
        for (g, s) in got.iter().zip(sung) {
            assert!((*s..*s + 60).contains(g), "{got:?} not on {sung:?}");
        }
    }

    fn starts(w: &LineWords) -> Vec<i64> {
        w.words.iter().map(|s| s.start_ms).collect()
    }

    #[test]
    fn an_anchored_line_moves_to_its_letters() {
        let lines = [
            line(1_000, 5_000, "grey gulls wheel"),
            line(5_000, 9_000, "low tide hums"),
        ];
        let mut sung = sing("grey gulls wheel", 1_500);
        sung.extend(sing("low tide hums", 5_600));
        let frames = 500;
        let e = emission(frames, &sung);
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        // The recogniser heard both lines a little late.
        let mut h = heard("grey gulls wheel", 1.7);
        h.extend(heard("low tide hums", 5.9));
        let m = match_lines(&lines, &h, Options::default());
        let t = [today(&lines[0], 1_000), today(&lines[1], 5_000)];
        let r = place(&lines, &t, &m, &song);
        assert_eq!(
            r.methods,
            vec![Some(Method::Refined), Some(Method::Refined)]
        );
        sung_at(&r.lines[0], &[1_500, 2_000, 2_600]);
        sung_at(&r.lines[1], &[5_600, 6_000, 6_500]);
        assert_eq!(r.placement.count(Method::Refined), 2);
        assert_eq!(r.placement.anchor_share, Some(1.0));
    }

    #[test]
    fn an_anchor_its_band_cannot_hold_keeps_the_recognisers_times() {
        let lines = [line(1_000, 5_000, "grey gulls wheel")];
        // The emission ends before the anchor does: no room for the letters.
        let frames = 60;
        let e = emission(frames, &[]);
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let m = match_lines(&lines, &heard("grey gulls wheel", 3.0), Options::default());
        let r = place(&lines, &[today(&lines[0], 1_000)], &m, &song);
        assert_eq!(r.methods, vec![Some(Method::Anchored)]);
        assert_eq!(starts(&r.lines[0]), vec![3_000, 3_500, 4_000, 4_400]);
    }

    #[test]
    fn lines_between_anchors_go_in_the_gap_and_others_keep_todays_time() {
        let lines = [
            line(1_000, 4_000, "grey gulls wheel"),
            line(4_000, 7_000, "nets dry slow"),
            line(7_000, 10_000, "low tide hums"),
            line(10_000, 13_000, "boats come home"),
        ];
        let mut sung = sing("grey gulls wheel", 1_000);
        sung.extend(sing("nets dry slow", 4_400));
        sung.extend(sing("low tide hums", 7_000));
        sung.extend(sing("boats come home", 10_000));
        let frames = 700;
        let e = emission(frames, &sung);
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        // Lines 1 and 3 were not heard; line 1 lies between two anchors.
        let mut h = heard("grey gulls wheel", 1.0);
        h.extend(heard("low tide hums", 7.0));
        let m = match_lines(&lines, &h, Options::default());
        // Today put line 1 on top of line 0, and line 3 somewhere wrong.
        let t = [
            today(&lines[0], 1_000),
            today(&lines[1], 1_500),
            today(&lines[2], 7_000),
            today(&lines[3], 12_000),
        ];
        let r = place(&lines, &t, &m, &song);
        assert_eq!(
            r.methods,
            vec![
                Some(Method::Refined),
                Some(Method::Between),
                Some(Method::Refined),
                Some(Method::Kept)
            ]
        );
        sung_at(&r.lines[1], &[4_400, 4_900, 5_300]);
        assert_eq!(r.lines[3], t[3]);
    }

    #[test]
    fn a_line_sung_twice_in_its_band_takes_the_one_nearer_its_anchor() {
        let lines = [line(1_000, 5_000, "grey gulls wheel")];
        // Sung at the anchor and again, a little more clearly, 1.3 s later.
        let near = sing("grey gulls wheel", 2_000);
        let mut sung = near.clone();
        sung.extend(sing("grey gulls wheel", 3_300));
        let frames = 400;
        let mut e = emission(frames, &sung);
        for &(c, ms) in &near {
            let (c, f) = (
                label(c).unwrap() as usize,
                usize::try_from(ms / FRAME_MS).unwrap(),
            );
            for t in f..f + 3 {
                e[t * CLASSES + c] = -0.015;
            }
        }
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let m = match_lines(&lines, &heard("grey gulls wheel", 2.0), Options::default());
        let r = place(&lines, &[today(&lines[0], 1_000)], &m, &song);
        assert_eq!(r.methods, vec![Some(Method::Refined)]);
        sung_at(&r.lines[0], &[2_000, 2_500, 3_100]);
    }

    #[test]
    fn a_gap_too_short_for_its_lines_keeps_the_recognisers_times() {
        let long = ["grey gulls wheel"; 25].join(" ");
        let lines = [
            line(1_000, 3_000, "grey gulls wheel"),
            line(3_000, 3_200, &long),
            line(3_200, 6_000, "low tide hums"),
        ];
        let frames = 500;
        let e = emission(frames, &[]);
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let mut h = heard("grey gulls wheel", 1.0);
        h.extend(heard("low tide hums", 3.5));
        let m = match_lines(&lines, &h, Options::default());
        let t: Vec<LineWords> = lines.iter().map(|l| today(l, l.start_ms)).collect();
        let r = place(&lines, &t, &m, &song);
        assert_eq!(
            r.methods,
            vec![
                Some(Method::Anchored),
                Some(Method::Kept),
                Some(Method::Anchored)
            ]
        );
        let a = m.lines[0].anchor.as_ref().unwrap();
        let mut want = a.starts_ms.clone();
        want.push(a.end_ms);
        assert_eq!(starts(&r.lines[0]), want);
        assert_eq!(r.lines[1], t[1]);
    }

    #[test]
    fn a_song_with_too_few_anchors_keeps_todays_answer() {
        let lines = [
            line(1_000, 4_000, "grey gulls wheel"),
            line(4_000, 7_000, "nets dry slow"),
            line(7_000, 10_000, "low tide hums"),
            line(10_000, 13_000, "♪"),
        ];
        let frames = 600;
        let e = emission(frames, &sing("grey gulls wheel", 1_500));
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let m = match_lines(&lines, &heard("grey gulls wheel", 1.5), Options::default());
        let t: Vec<LineWords> = lines.iter().map(|l| today(l, l.start_ms)).collect();
        let r = place(&lines, &t, &m, &song);
        assert_eq!(r.lines, t);
        assert_eq!(r.methods[3], None);
        assert_eq!(r.placement.count(Method::Kept), 3);
        assert_eq!(r.placement.fallback, Some(Fallback::LowAnchorShare));
        assert_eq!(r.placement.anchor_share, Some(1.0 / 3.0));
    }

    #[test]
    fn an_anchor_far_from_its_neighbours_is_dropped() {
        let lines = [
            line(1_000, 4_000, "grey gulls wheel"),
            line(4_000, 7_000, "nets dry slow"),
            line(7_000, 10_000, "low tide hums"),
            line(10_000, 13_000, "boats come home"),
        ];
        let mut sung = sing("grey gulls wheel", 1_200);
        sung.extend(sing("nets dry slow", 4_200));
        sung.extend(sing("low tide hums", 7_200));
        // The last line heard sung again twelve seconds late.
        sung.extend(sing("low tide hums", 19_200));
        let frames = 1_100;
        let e = emission(frames, &sung);
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let mut h = heard("grey gulls wheel", 1.2);
        h.extend(heard("nets dry slow", 4.2));
        h.extend(heard("low tide hums", 19.2));
        let m = match_lines(&lines, &h, Options::default());
        assert!(m.lines[2].anchor.is_some());
        let t: Vec<LineWords> = lines.iter().map(|l| today(l, l.start_ms)).collect();
        let r = place(&lines, &t, &m, &song);
        assert_eq!(
            r.methods,
            vec![
                Some(Method::Refined),
                Some(Method::Refined),
                Some(Method::Kept),
                Some(Method::Kept)
            ]
        );
        assert_eq!(r.lines[2], t[2]);
        assert_eq!(r.placement.dropped_anchors, 1);
        assert_eq!(r.placement.anchor_share, Some(0.5));
    }

    #[test]
    fn a_line_before_the_first_anchor_that_runs_into_it_is_placed_before_it() {
        let lines = [
            line(1_000, 4_000, "grey gulls wheel"),
            line(4_000, 7_000, "nets dry slow"),
            line(7_000, 10_000, "low tide hums"),
            line(10_000, 13_000, "boats come home"),
        ];
        let mut sung = sing("grey gulls wheel", 1_000);
        sung.extend(sing("nets dry slow", 4_000));
        sung.extend(sing("low tide hums", 7_000));
        sung.extend(sing("boats come home", 10_000));
        let frames = 700;
        let e = emission(frames, &sung);
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let mut h = heard("nets dry slow", 4.0);
        h.extend(heard("low tide hums", 7.0));
        let m = match_lines(&lines, &h, Options::default());
        // Today ran line 0 on into line 1; line 3 it left well clear.
        let t = [
            today(&lines[0], 3_500),
            today(&lines[1], 4_000),
            today(&lines[2], 7_000),
            today(&lines[3], 12_000),
        ];
        let r = place(&lines, &t, &m, &song);
        assert_eq!(
            r.methods,
            vec![
                Some(Method::Between),
                Some(Method::Refined),
                Some(Method::Refined),
                Some(Method::Kept)
            ]
        );
        sung_at(&r.lines[0], &[1_000, 1_500, 2_100]);
        assert_eq!(r.lines[3], t[3]);
    }

    #[test]
    fn an_anchor_far_from_its_neighbour_that_leaps_no_line_is_kept() {
        // The request has the last line twelve seconds early, and nothing
        // after it to leap past: the request is off, not the recogniser.
        let lines = [
            line(1_000, 4_000, "grey gulls wheel"),
            line(4_000, 7_000, "nets dry slow"),
            line(7_000, 10_000, "low tide hums"),
        ];
        let mut sung = sing("grey gulls wheel", 1_200);
        sung.extend(sing("nets dry slow", 4_200));
        sung.extend(sing("low tide hums", 19_200));
        let frames = 1_100;
        let e = emission(frames, &sung);
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let mut h = heard("grey gulls wheel", 1.2);
        h.extend(heard("nets dry slow", 4.2));
        h.extend(heard("low tide hums", 19.2));
        let m = match_lines(&lines, &h, Options::default());
        let t: Vec<LineWords> = lines.iter().map(|l| today(l, l.start_ms)).collect();
        let r = place(&lines, &t, &m, &song);
        assert_eq!(r.methods[2], Some(Method::Refined));
        sung_at(&r.lines[2], &[19_200, 19_600, 20_100]);
        assert_eq!(r.placement.dropped_anchors, 0);
    }

    #[test]
    fn a_line_after_the_last_anchor_that_runs_back_into_it_is_placed_after_it() {
        let lines = [
            line(1_000, 4_000, "grey gulls wheel"),
            line(4_000, 7_000, "nets dry slow"),
            line(7_000, 10_000, "low tide hums"),
            line(10_000, 13_000, "boats come home"),
        ];
        let mut sung = sing("grey gulls wheel", 1_000);
        sung.extend(sing("nets dry slow", 4_000));
        sung.extend(sing("low tide hums", 7_000));
        sung.extend(sing("boats come home", 10_000));
        let frames = 700;
        let e = emission(frames, &sung);
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let mut h = heard("grey gulls wheel", 1.0);
        h.extend(heard("nets dry slow", 4.0));
        let m = match_lines(&lines, &h, Options::default());
        // Today started line 2 inside line 1; line 3 it left well clear.
        let t = [
            today(&lines[0], 1_000),
            today(&lines[1], 4_000),
            today(&lines[2], 5_000),
            today(&lines[3], 12_000),
        ];
        let r = place(&lines, &t, &m, &song);
        assert_eq!(
            r.methods,
            vec![
                Some(Method::Refined),
                Some(Method::Refined),
                Some(Method::Between),
                Some(Method::Kept)
            ]
        );
        sung_at(&r.lines[2], &[7_000, 7_400, 7_900]);
        assert_eq!(r.lines[3], t[3]);
    }
}
