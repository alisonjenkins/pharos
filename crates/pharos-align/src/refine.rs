//! Word times from the recogniser's anchors (spec 042, plan steps 5 and 6):
//! each anchored line refined letter by letter on the vocals' emission
//! inside its anchor, lines between two anchors aligned in today's window
//! clamped to the gap between them, and every other line left as today
//! placed it.

use crate::anchor::Matched;
use crate::ctc::{align_words, Emission};
use crate::line::frame_ms;
use crate::placement::{Fallback, Method, Placement};
use crate::plan::{self, Line, Stamp};
use crate::service::LineWords;
use crate::text::{self, is_py_space};
use crate::tokens::{tokenize, STAR_TOKEN};

/// Audio either side of an anchor the letters may move into.
pub const PAD_MS: i64 = 1_500;
/// The recogniser often ends a line's last word early; its band runs at
/// least this far past where that word starts.
pub const LAST_WORD_MS: i64 = 400;
/// Below this share of lines anchored the song keeps today's answer whole:
/// the prototype's very repetitive songs (chants, "na na") fell under 40%
/// and their few anchors were the least trustworthy.
pub const MIN_ANCHOR_SHARE: f64 = 0.4;

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

    /// The frame holding `ms`, clamped to the song.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn frame(&self, ms: i64) -> usize {
        ((ms.max(0) as f64 / self.ms_per_frame()) as usize).min(self.emission.frames())
    }
}

/// Every line's words and how they were placed; `methods` is `None` for a line with no words.
#[derive(Debug, Clone, PartialEq)]
pub struct Refined {
    pub lines: Vec<LineWords>,
    pub methods: Vec<Option<Method>>,
    pub placement: Placement,
}

fn first_ms(w: &LineWords) -> Option<i64> {
    w.words.first().map(|s| s.start_ms)
}

fn end_ms(w: &LineWords) -> Option<i64> {
    w.words.last().map(|s| s.start_ms)
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

/// The line's letters aligned within `[lo_ms, hi_ms)` of the song, between
/// a leading and trailing star; `None` when the band cannot hold them.
fn align_in(
    song: &Song<'_>,
    line: &Line,
    words: &[text::Word],
    lo_ms: i64,
    hi_ms: i64,
) -> Option<LineWords> {
    let (lo, hi) = (song.frame(lo_ms), song.frame(hi_ms));
    if hi <= lo {
        return None;
    }
    let band = song.emission.frames_between(lo, hi);
    let mut tokens: Vec<&str> = vec![STAR_TOKEN];
    tokens.extend(words.iter().map(|w| w.token.as_str()));
    tokens.push(STAR_TOKEN);
    let tokenized = tokenize(&tokens).ok()?;
    let spans = align_words(&band, &tokenized).ok()?;
    let letters = &spans[1..spans.len() - 1];
    let times: Vec<(u64, u64)> = letters
        .iter()
        .map(|w| match (w.first(), w.last()) {
            (Some(f), Some(l)) => (
                frame_ms(song.n_samples, song.emission.frames(), lo + f.start),
                frame_ms(song.n_samples, song.emission.frames(), lo + l.end),
            ),
            _ => (0, 0),
        })
        .collect();
    let scores: Vec<f64> = letters
        .iter()
        .flatten()
        .map(|c| f64::from(c.score))
        .collect();
    let score = (!scores.is_empty()).then(|| scores.iter().sum::<f64>() / scores.len() as f64);
    let stamps = plan::stamps(&line.text, words, &times, 0);
    (!stamps.is_empty()).then(|| LineWords {
        words: stamps,
        score: score.and_then(crate::service::round_score),
    })
}

/// Places every line: `today` is today's per-line answer and `offset_ms` the
/// offset it found, `matched` the recogniser's anchors.
pub fn place(
    lines: &[Line],
    today: &[LineWords],
    offset_ms: i64,
    matched: &Matched,
    song: &Song<'_>,
) -> Refined {
    let words: Vec<Vec<text::Word>> = lines.iter().map(|l| text::words(&l.text)).collect();
    let with_words = words.iter().filter(|w| !w.is_empty()).count();
    let share = matched.share().unwrap_or(0.0);
    let mut out: Vec<LineWords> = today.to_vec();
    let mut methods: Vec<Option<Method>> = words
        .iter()
        .map(|w| (!w.is_empty()).then_some(Method::Kept))
        .collect();
    if share < MIN_ANCHOR_SHARE {
        let mut placement = Placement::kept(with_words, Fallback::LowAnchorShare);
        placement.anchor_share = Some(share);
        return Refined {
            lines: out,
            methods,
            placement,
        };
    }

    let anchored: Vec<usize> = (0..lines.len())
        .filter(|&i| {
            !words[i].is_empty() && matched.lines.get(i).is_some_and(|m| m.anchor.is_some())
        })
        .collect();
    let mut prev_end = i64::MIN;
    for &i in &anchored {
        let Some(a) = matched.lines[i].anchor.as_ref() else {
            continue;
        };
        let first = a.starts_ms.first().copied().unwrap_or(a.end_ms);
        let last = a.starts_ms.last().copied().unwrap_or(a.end_ms);
        let lo = (first - PAD_MS).max(prev_end);
        let hi = a.end_ms.max(last + LAST_WORD_MS) + PAD_MS;
        let (w, m) = match align_in(song, &lines[i], &words[i], lo, hi) {
            Some(w) => (w, Method::Refined),
            None => (
                anchor_words(&lines[i], &words[i], &a.starts_ms, a.end_ms),
                Method::Anchored,
            ),
        };
        prev_end = end_ms(&w).unwrap_or(prev_end);
        out[i] = w;
        methods[i] = Some(m);
    }

    for pair in anchored.windows(2) {
        let (p, q) = (pair[0], pair[1]);
        let mut gap_lo = end_ms(&out[p]).unwrap_or(i64::MIN);
        let gap_hi = first_ms(&out[q]).unwrap_or(i64::MAX);
        for i in p + 1..q {
            if words[i].is_empty() {
                continue;
            }
            let inside = first_ms(&out[i]).is_some_and(|s| s >= gap_lo)
                && end_ms(&out[i]).is_some_and(|e| e <= gap_hi);
            if !inside {
                let shifted = Line {
                    start_ms: lines[i].start_ms + offset_ms,
                    end_ms: lines[i].end_ms + offset_ms,
                    text: lines[i].text.clone(),
                };
                let (ws, we) = plan::window(&shifted, words[i].len(), gap_lo);
                if let Some(w) =
                    align_in(song, &lines[i], &words[i], ws.max(gap_lo), we.min(gap_hi))
                {
                    out[i] = w;
                    methods[i] = Some(Method::Between);
                }
            }
            gap_lo = end_ms(&out[i]).unwrap_or(gap_lo).max(gap_lo);
        }
    }

    let placement = Placement::from_methods(methods.iter().flatten().copied(), share);
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
        let r = place(&lines, &t, 0, &m, &song);
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
        let r = place(&lines, &[today(&lines[0], 1_000)], 0, &m, &song);
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
        let r = place(&lines, &t, 0, &m, &song);
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
    fn a_line_today_already_put_inside_its_gap_is_kept() {
        let lines = [
            line(1_000, 4_000, "grey gulls wheel"),
            line(4_000, 7_000, "nets dry slow"),
            line(7_000, 10_000, "low tide hums"),
        ];
        let frames = 600;
        let e = emission(frames, &sing("grey gulls wheel", 1_000));
        let song = Song {
            emission: Emission::new(&e, frames, CLASSES).unwrap(),
            n_samples: frames * SAMPLES_PER_FRAME,
        };
        let mut h = heard("grey gulls wheel", 1.0);
        h.extend(heard("low tide hums", 7.0));
        let m = match_lines(&lines, &h, Options::default());
        let t = [
            today(&lines[0], 1_000),
            today(&lines[1], 4_500),
            today(&lines[2], 7_000),
        ];
        let r = place(&lines, &t, 0, &m, &song);
        assert_eq!(r.methods[1], Some(Method::Kept));
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
        let r = place(&lines, &t, 0, &m, &song);
        assert_eq!(r.lines, t);
        assert_eq!(r.methods[3], None);
        assert_eq!(r.placement.count(Method::Kept), 3);
        assert_eq!(r.placement.fallback, Some(Fallback::LowAnchorShare));
        assert_eq!(r.placement.anchor_share, Some(1.0 / 3.0));
    }
}
