//! Lyric lines anchored on the recogniser's words (spec 042, plan step 4):
//! a monotonic match of lyric words to heard words, then a letter-level
//! pass for lines the word match missed. A port of the prototype's
//! `match.py`, with the slot cap added to both passes.

use std::collections::HashMap;

use crate::plan::Line;
use crate::text::is_py_space;
use crate::transcript::Word as Heard;

/// Scores of the word match: leaving a lyric word or a heard word unmatched.
const SKIP_LYRIC: f64 = -0.4;
const SKIP_HEARD: f64 = -0.3;
/// Letter similarity a lyric and heard word need to match at all, and to count towards an anchor.
const MATCH_MIN: f64 = 0.6;
const STRONG_MIN: f64 = 0.8;
/// Tie-break between repeats of the same words: score lost per second a
/// heard word lies from its line's stamp, capped so a wrong stamp cannot
/// outvote the words.
const STAMP_COST_PER_S: f64 = 0.005;
const STAMP_COST_CAP: f64 = 0.3;
/// Letter similarity of a whole line to a run of heard words in the second pass.
const SOFT_MIN: f64 = 0.6;
/// The second pass looks this far back before where the last anchor ended.
const SOFT_BACK_S: f64 = 0.5;
/// Spacing given to unmatched words before the first or after the last matched word.
const FILL_MS_PER_WORD: i64 = 300;
/// An anchor whose word starts spread wider than this many times its stamp
/// slot, plus `SLOT_CAP_EXTRA_MS`, took its words from another repeat of
/// the same text; the line is left unanchored.
const SLOT_CAP_FACTOR: f64 = 2.0;
const SLOT_CAP_EXTRA_MS: f64 = 3000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Off only to compare with the prototype, which had no cap.
    pub slot_cap: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { slot_cap: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pass {
    /// At least half the line's words, and at least two, matched heard words.
    Words,
    /// The line's letters matched a run of heard words left over by the word pass.
    Letters { ratio: f64 },
}

/// Where the recogniser heard a line.
#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub pass: Pass,
    /// One start per whitespace-separated word of the line text, non-decreasing.
    pub starts_ms: Vec<i64>,
    pub end_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LineMatch {
    /// Words of the line with letters or digits; 0 means the line has nothing to match.
    pub words: usize,
    /// Words matched closely enough to count towards an anchor.
    pub strong: usize,
    pub anchor: Option<Anchor>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Matched {
    pub lines: Vec<LineMatch>,
}

impl Matched {
    /// Anchored lines over lines with words; `None` when no line has words.
    pub fn share(&self) -> Option<f64> {
        let with = self.lines.iter().filter(|l| l.words > 0).count();
        let anchored = self.lines.iter().filter(|l| l.anchor.is_some()).count();
        (with > 0).then(|| anchored as f64 / with as f64)
    }
}

/// Python's `str.split()`.
pub(crate) fn split_words(text: &str) -> Vec<&str> {
    text.split(is_py_space).filter(|w| !w.is_empty()).collect()
}

/// Lowercase letters and digits only, as both sides are compared.
fn norm(word: &str) -> Vec<u8> {
    word.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .map(|c| c as u8)
        .collect()
}

/// `difflib.SequenceMatcher(None, a, b).ratio()`, autojunk included.
pub(crate) fn ratio(a: &[u8], b: &[u8]) -> f64 {
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    let mut b2j: HashMap<u8, Vec<usize>> = HashMap::new();
    for (j, &c) in b.iter().enumerate() {
        b2j.entry(c).or_default().push(j);
    }
    if b.len() >= 200 {
        let popular = b.len() / 100 + 1;
        b2j.retain(|_, js| js.len() <= popular);
    }
    let mut matched = 0;
    let mut queue = vec![(0, a.len(), 0, b.len())];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest_match(a, b, &b2j, (alo, ahi, blo, bhi));
        if k > 0 {
            matched += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    2.0 * matched as f64 / total as f64
}

/// `SequenceMatcher.find_longest_match` with no junk function: popular
/// elements cannot start a match but still extend one.
fn longest_match(
    a: &[u8],
    b: &[u8],
    b2j: &HashMap<u8, Vec<usize>>,
    (alo, ahi, blo, bhi): (usize, usize, usize, usize),
) -> (usize, usize, usize) {
    let (mut bi, mut bj, mut size) = (alo, blo, 0);
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for (i, c) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut next = HashMap::new();
        for &j in b2j.get(c).map_or(&[][..], Vec::as_slice) {
            if j < blo {
                continue;
            }
            if j >= bhi {
                break;
            }
            let k = j
                .checked_sub(1)
                .and_then(|p| j2len.get(&p))
                .copied()
                .unwrap_or(0)
                + 1;
            next.insert(j, k);
            if k > size {
                (bi, bj, size) = (i + 1 - k, j + 1 - k, k);
            }
        }
        j2len = next;
    }
    while bi > alo && bj > blo && a[bi - 1] == b[bj - 1] {
        (bi, bj, size) = (bi - 1, bj - 1, size + 1);
    }
    while bi + size < ahi && bj + size < bhi && a[bi + size] == b[bj + size] {
        size += 1;
    }
    (bi, bj, size)
}

fn sim(a: &[u8], b: &[u8]) -> f64 {
    if a == b {
        1.0
    } else {
        ratio(a, b)
    }
}

struct Lyric {
    line: usize,
    word: usize,
    norm: Vec<u8>,
}

struct Token {
    norm: Vec<u8>,
    start_s: f64,
    end_s: f64,
}

#[allow(clippy::cast_possible_truncation)]
fn ms(s: f64) -> i64 {
    (s * 1000.0) as i64
}

/// Too wide for the line's stamp slot: the words came from another repeat.
fn over_slot(line: &Line, first_s: f64, last_s: f64) -> bool {
    let slot_s = (line.end_ms - line.start_ms) as f64 / 1000.0;
    last_s - first_s > SLOT_CAP_FACTOR * slot_s + SLOT_CAP_EXTRA_MS / 1000.0
}

/// Fills unmatched words between matched ones by interpolation and before
/// or after them at `FILL_MS_PER_WORD`, then makes the starts non-decreasing.
fn fill(starts: &[Option<i64>]) -> Vec<i64> {
    let known: Vec<usize> = (0..starts.len()).filter(|&k| starts[k].is_some()).collect();
    let at = |k: usize| starts[k].unwrap_or_default();
    let mut out = Vec::with_capacity(starts.len());
    for (k, start) in starts.iter().enumerate() {
        let t = start.unwrap_or_else(|| {
            let a = known.iter().rev().find(|&&x| x < k).copied();
            let b = known.iter().find(|&&x| x > k).copied();
            match (a, b) {
                (Some(a), Some(b)) => {
                    at(a) + ((at(b) - at(a)) * (k - a) as i64).div_euclid((b - a) as i64)
                }
                (Some(a), None) => at(a) + FILL_MS_PER_WORD * (k - a) as i64,
                (None, Some(b)) => at(b) - FILL_MS_PER_WORD * (b - k) as i64,
                (None, None) => 0,
            }
        });
        out.push(out.last().map_or(t, |&l: &i64| l.max(t)));
    }
    out
}

/// Anchors `lines` on `heard`, in lyric order.
pub fn match_lines(lines: &[Line], heard: &[Heard], opts: Options) -> Matched {
    let split: Vec<Vec<&str>> = lines.iter().map(|l| split_words(&l.text)).collect();
    let lyr: Vec<Lyric> = split
        .iter()
        .enumerate()
        .flat_map(|(line, ws)| {
            ws.iter().enumerate().filter_map(move |(word, w)| {
                let norm = norm(w);
                (!norm.is_empty()).then_some(Lyric { line, word, norm })
            })
        })
        .collect();
    let tr: Vec<Token> = heard
        .iter()
        .filter_map(|h| {
            let norm = norm(&h.text);
            (!norm.is_empty()).then_some(Token {
                norm,
                start_s: h.start_s,
                end_s: h.end_s,
            })
        })
        .collect();
    let pairs = word_match(lines, &lyr, &tr);

    let mut out = Matched {
        lines: vec![LineMatch::default(); lines.len()],
    };
    let mut used = vec![false; tr.len()];
    for (li, line) in lines.iter().enumerate() {
        let ks: Vec<usize> = (0..lyr.len()).filter(|&q| lyr[q].line == li).collect();
        if ks.is_empty() {
            continue;
        }
        let hits: Vec<(usize, usize)> = ks
            .iter()
            .filter_map(|&q| pairs[q].map(|j| (q, j)))
            .collect();
        let strong: Vec<(usize, usize)> = hits
            .iter()
            .copied()
            .filter(|&(q, j)| sim(&lyr[q].norm, &tr[j].norm) >= STRONG_MIN)
            .collect();
        let m = &mut out.lines[li];
        m.words = ks.len();
        m.strong = strong.len();
        let frac = strong.len() as f64 / ks.len() as f64;
        let mut anchored = strong.len() >= ks.len().min(2) && frac >= 0.5;
        if anchored && opts.slot_cap {
            let s = strong.iter().map(|&(_, j)| tr[j].start_s);
            let first = s.clone().fold(f64::INFINITY, f64::min);
            let last = s.fold(f64::NEG_INFINITY, f64::max);
            anchored = !over_slot(line, first, last);
        }
        if !anchored {
            continue;
        }
        let mut starts = vec![None; split[li].len()];
        for &(q, j) in &hits {
            starts[lyr[q].word] = Some(ms(tr[j].start_s));
            used[j] = true;
        }
        let end_s = hits
            .iter()
            .map(|&(_, j)| tr[j].end_s)
            .fold(f64::NEG_INFINITY, f64::max);
        m.anchor = Some(Anchor {
            pass: Pass::Words,
            starts_ms: fill(&starts),
            end_ms: ms(end_s),
        });
    }
    letter_pass(lines, &split, &lyr, &tr, &used, opts, &mut out);
    out
}

/// The word-level DP: for each lyric word, the heard word it matched.
fn word_match(lines: &[Line], lyr: &[Lyric], tr: &[Token]) -> Vec<Option<usize>> {
    let (n, m) = (lyr.len(), tr.len());
    let w = m + 1;
    let mut d = vec![0.0f64; (n + 1) * w];
    let mut back = vec![0u8; (n + 1) * w];
    for i in 1..=n {
        d[i * w] = d[(i - 1) * w] + SKIP_LYRIC;
        back[i * w] = 1;
        let stamp_s = lines[lyr[i - 1].line].start_ms as f64 / 1000.0;
        for j in 1..=m {
            let (mut best, mut b) = (d[(i - 1) * w + j] + SKIP_LYRIC, 1);
            if d[i * w + j - 1] + SKIP_HEARD > best {
                (best, b) = (d[i * w + j - 1] + SKIP_HEARD, 2);
            }
            let s = sim(&lyr[i - 1].norm, &tr[j - 1].norm);
            if s >= MATCH_MIN {
                let off =
                    (STAMP_COST_PER_S * (tr[j - 1].start_s - stamp_s).abs()).min(STAMP_COST_CAP);
                let g = 2.0 * s - 1.0 - off;
                if d[(i - 1) * w + j - 1] + g > best {
                    (best, b) = (d[(i - 1) * w + j - 1] + g, 3);
                }
            }
            d[i * w + j] = best;
            back[i * w + j] = b;
        }
    }
    let mut j = 0;
    for k in 1..=m {
        if d[n * w + k] > d[n * w + j] {
            j = k;
        }
    }
    let mut pairs = vec![None; n];
    let mut i = n;
    while i > 0 {
        match back[i * w + j] {
            3 => {
                pairs[i - 1] = Some(j - 1);
                i -= 1;
                j -= 1;
            }
            1 => i -= 1,
            _ => j -= 1,
        }
    }
    pairs
}

/// A line the word pass missed, between where the last anchor ended and
/// where the next begins, matched on letters against a run of heard words
/// no anchor used: the first run, in order, within one word of the line's
/// length and at least `SOFT_MIN` alike.
fn letter_pass(
    lines: &[Line],
    split: &[Vec<&str>],
    lyr: &[Lyric],
    tr: &[Token],
    used: &[bool],
    opts: Options,
    out: &mut Matched,
) {
    let mut free: Vec<usize> = (0..tr.len()).filter(|&k| !used[k]).collect();
    let mut cur = -1.0f64;
    for li in 0..lines.len() {
        if out.lines[li].words == 0 {
            continue;
        }
        if let Some(a) = &out.lines[li].anchor {
            let end = if a.end_ms != 0 {
                a.end_ms
            } else {
                a.starts_ms.last().copied().unwrap_or_default()
            };
            cur = end as f64 / 1000.0;
            continue;
        }
        let lim = out.lines[li + 1..]
            .iter()
            .find_map(|l| l.anchor.as_ref())
            .and_then(|a| a.starts_ms.first())
            .map_or(1e9, |&s| s as f64 / 1000.0);
        let target: Vec<u8> = lyr
            .iter()
            .filter(|x| x.line == li)
            .flat_map(|x| x.norm.iter().copied())
            .collect();
        let nw = out.lines[li].words;
        let cand: Vec<usize> = free
            .iter()
            .copied()
            .filter(|&k| cur - SOFT_BACK_S <= tr[k].start_s && tr[k].start_s < lim)
            .collect();
        for a in 0..cand.len() {
            let mut got: Option<(f64, &[usize])> = None;
            for len in [nw - 1, nw, nw + 1] {
                if len < 1 || a + len > cand.len() {
                    continue;
                }
                let win = &cand[a..a + len];
                let (first, last) = (&tr[win[0]], &tr[win[len - 1]]);
                if last.start_s >= lim
                    || (opts.slot_cap && over_slot(&lines[li], first.start_s, last.start_s))
                {
                    continue;
                }
                let heard: Vec<u8> = win
                    .iter()
                    .flat_map(|&k| tr[k].norm.iter().copied())
                    .collect();
                let rt = ratio(&target, &heard);
                if rt >= SOFT_MIN && got.map_or(true, |(g, _)| rt > g) {
                    got = Some((rt, win));
                }
            }
            let Some((rt, win)) = got else { continue };
            let st = ms(tr[win[0]].start_s);
            let en = ms(tr[win[win.len() - 1]].end_s);
            let nwt = split[li].len().max(1) as i64;
            let starts_ms = (0..split[li].len() as i64)
                .map(|k| st + ((en - st) * k).div_euclid(nwt))
                .collect();
            out.lines[li].anchor = Some(Anchor {
                pass: Pass::Letters { ratio: rt },
                starts_ms,
                end_ms: en,
            });
            let win = win.to_vec();
            free.retain(|k| !win.contains(k));
            cur = en as f64 / 1000.0;
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn line(start_ms: i64, end_ms: i64, text: &str) -> Line {
        Line {
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    fn heard(words: &[(&str, f64)]) -> Vec<Heard> {
        words
            .iter()
            .map(|&(w, s)| Heard {
                text: format!(" {w}"),
                start_s: s,
                end_s: s + 0.3,
                prob: 0.9,
            })
            .collect()
    }

    /// Values from CPython 3.13's difflib.
    #[test]
    fn ratio_matches_difflib() {
        let r = |a: &str, b: &str| ratio(a.as_bytes(), b.as_bytes());
        assert_eq!(r("abcd", "bcde"), 0.75);
        assert_eq!(r("hello", "hallo"), 0.8);
        assert_eq!(r("abab", "baba"), 0.75);
        assert_eq!(r("", ""), 1.0);
        assert_eq!(r("", "x"), 0.0);
        assert_eq!(r("qwertyuiopasdfgh", "qwyuiopzzasdfgh"), 0.8387096774193549);
        // Autojunk: in a b of 200 or more, letters seen too often cannot
        // start a match, but still extend one.
        let b = format!("{}needle", "ab".repeat(100));
        assert_eq!(r("aaneedlebb", &b), 0.06481481481481481);
        let b = format!("xyz{}needlea", "ab".repeat(100));
        assert_eq!(r("xyzaneedleab", &b), 0.0990990990990991);
    }

    #[test]
    fn heard_words_anchor_their_lines_and_fill_the_gaps() {
        let lines = [
            line(10_000, 14_000, "Paper boats on the river"),
            line(14_000, 19_000, "drifting under lantern light"),
        ];
        let h = heard(&[
            ("thank", 2.0),
            ("paper", 10.5),
            ("boats", 11.0),
            ("on", 11.4),
            ("river", 12.6),
            ("drifting", 14.9),
            ("under", 15.6),
            ("lanterns", 16.1),
            ("light", 16.9),
        ]);
        let m = match_lines(&lines, &h, Options::default());
        let a = m.lines[0].anchor.as_ref().unwrap();
        assert_eq!(a.pass, Pass::Words);
        // "the" was not heard: halfway between "on" and "river".
        assert_eq!(a.starts_ms, vec![10_500, 11_000, 11_400, 12_000, 12_600]);
        assert_eq!(a.end_ms, 12_900);
        let b = m.lines[1].anchor.as_ref().unwrap();
        assert_eq!(b.starts_ms, vec![14_900, 15_600, 16_100, 16_900]);
        assert_eq!(b.end_ms, 17_200);
        assert_eq!(m.share(), Some(1.0));
    }

    #[test]
    fn a_repeated_line_takes_the_repeat_nearest_its_stamp() {
        let chorus = "hold the lantern high";
        let lines = [
            line(5_000, 9_000, chorus),
            line(9_000, 30_000, "and the tide comes in"),
            line(30_000, 34_000, chorus),
        ];
        // The first line was heard twice; the stamp decides which is which.
        let h = heard(&[
            ("hold", 5.2),
            ("the", 5.6),
            ("lantern", 6.0),
            ("high", 6.8),
            ("hold", 30.4),
            ("the", 30.8),
            ("lantern", 31.2),
            ("high", 32.0),
        ]);
        let m = match_lines(&lines, &h, Options::default());
        assert_eq!(m.lines[0].anchor.as_ref().unwrap().starts_ms[0], 5_200);
        assert!(m.lines[1].anchor.is_none());
        let c = m.lines[2].anchor.as_ref().unwrap();
        assert_eq!((c.starts_ms[0], c.end_ms), (30_400, 32_299));
        assert_eq!(m.share(), Some(2.0 / 3.0));
    }

    #[test]
    fn words_spread_far_past_the_slot_do_not_anchor() {
        let lines = [line(20_000, 24_000, "salt and silver")];
        // Two words of the line from one repeat, one from another 30 s on.
        let h = heard(&[("salt", 20.2), ("silver", 50.0)]);
        let capped = match_lines(&lines, &h, Options::default());
        assert!(capped.lines[0].anchor.is_none());
        assert_eq!(capped.lines[0].strong, 2);
        assert_eq!(capped.share(), Some(0.0));
        let uncapped = match_lines(&lines, &h, Options { slot_cap: false });
        assert!(uncapped.lines[0].anchor.is_some());
    }

    #[test]
    fn a_misheard_line_between_anchors_is_matched_on_letters() {
        let lines = [
            line(1_000, 4_000, "over the harbour wall"),
            line(4_000, 7_000, "we fold the maps"),
            line(7_000, 10_000, "under a copper moon"),
        ];
        // Only "them" is close to its lyric word: one of four is too few
        // for a word anchor, but the letters of the run are alike enough.
        let h = heard(&[
            ("over", 1.1),
            ("the", 1.5),
            ("harbour", 1.9),
            ("wall", 2.6),
            ("with", 4.2),
            ("olde", 4.5),
            ("them", 5.0),
            ("apps", 5.4),
            ("under", 7.1),
            ("a", 7.5),
            ("copper", 7.8),
            ("moon", 8.4),
        ]);
        let m = match_lines(&lines, &h, Options::default());
        let a = m.lines[1].anchor.as_ref().unwrap();
        assert!(
            matches!(a.pass, Pass::Letters { ratio } if (ratio - 0.76).abs() < 0.005),
            "{a:?}"
        );
        assert_eq!(a.starts_ms[0], 4_200);
        assert_eq!(a.end_ms, 5_700);
        // Spread evenly over the run: (5700 - 4200) / 4 apart.
        assert_eq!(a.starts_ms, vec![4_200, 4_575, 4_950, 5_325]);
    }

    #[test]
    fn lines_without_letters_are_left_out_of_the_share() {
        let lines = [
            line(0, 1_000, "♪ ♪"),
            line(1_000, 3_000, "count the herons"),
        ];
        let m = match_lines(&lines, &[], Options::default());
        assert_eq!(m.lines[0].words, 0);
        assert_eq!(m.share(), Some(0.0));
        assert_eq!(
            match_lines(&lines[..1], &[], Options::default()).share(),
            None
        );
    }

    #[test]
    fn fill_interpolates_with_floor_division_and_never_goes_back() {
        assert_eq!(
            fill(&[None, Some(1_000), None, None, Some(1_001), Some(900), None]),
            vec![700, 1_000, 1_000, 1_000, 1_001, 1_001, 1_200]
        );
    }
}
