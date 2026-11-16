//! FR-004 / FR-005: the cues sent to the aligner, and their decoys.

use crate::model::Script;

/// FR-004: at most this many probes per track.
pub const MAX_PROBES: usize = 12;
/// FR-004: fewer usable probes than this and the track is `unknown`.
pub const MIN_PROBES: usize = 6;
/// FR-004: words the aligner can place (the aligner's own `MIN_PROBE_WORDS`).
pub const MIN_WORDS: usize = 3;
/// FR-004: shorter cues are too short to score.
pub const MIN_CUE_MS: i64 = 600;
/// FR-005: a decoy comes from at least this far away.
pub const DECOY_DISTANCE_MS: i64 = 10 * 60 * 1000;

/// One cue to align, with the text of a far-away cue to align in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub start_ms: i64,
    pub end_ms: i64,
    /// The cue's text, cleaned of SDH tags and speaker labels, on one line.
    pub text: String,
    /// FR-005: another eligible cue's text, aligned in this probe's window.
    pub decoy: String,
}

/// Why a track got no probe set: `too_few_lines:<n>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooFewLines(pub usize);

impl std::fmt::Display for TooFewLines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "too_few_lines:{}", self.0)
    }
}

/// Words in a style name that mark it as signs, songs or typesetting.
const SIGN_STYLE_WORDS: &[&str] = &[
    "sign", "signs", "song", "songs", "op", "ed", "opening", "ending", "karaoke", "kara", "lyric",
    "lyrics", "title", "titles", "typeset", "ts", "insert",
];

fn is_sign_style(name: &str) -> bool {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| SIGN_STYLE_WORDS.contains(&w) || w.starts_with("sign"))
}

fn has_music_note(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(c, '♪' | '♫' | '♬' | '♩' | '🎵' | '🎶'))
}

/// Drops `[...]` anywhere, a line that is only `(...)`, a leading dash and a
/// leading upper-case speaker label (`NAME:` / `MAN 2:`). Joins lines with a
/// space and collapses whitespace.
pub fn clean_text(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let mut kept = String::with_capacity(line.len());
        let mut depth = 0u32;
        for c in line.chars() {
            match c {
                '[' => depth += 1,
                ']' if depth > 0 => depth -= 1,
                _ if depth == 0 => kept.push(c),
                _ => {}
            }
        }
        let mut line = kept.trim().trim_start_matches(['-', '–', '—']).trim();
        if line.starts_with('(') && line.ends_with(')') {
            continue;
        }
        if let Some((label, rest)) = line.split_once(':') {
            let is_label = !label.is_empty()
                && label.chars().any(char::is_alphabetic)
                && label.chars().all(|c| {
                    c.is_uppercase() || c.is_ascii_digit() || matches!(c, ' ' | '.' | '\'' | '-')
                });
            if is_label {
                line = rest.trim();
            }
        }
        if !line.is_empty() {
            out.push(line.to_owned());
        }
    }
    out.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words with at least one letter: what the aligner can place.
pub fn alignable_words(text: &str) -> usize {
    text.split_whitespace()
        .filter(|w| w.chars().any(char::is_alphabetic))
        .count()
}

/// Lower-case letters and digits only, for FR-004's uniqueness test.
fn key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

struct Eligible {
    start_ms: i64,
    end_ms: i64,
    text: String,
    unique: bool,
}

fn eligible(script: &Script) -> Vec<Eligible> {
    let mut cues: Vec<Eligible> = script
        .events
        .iter()
        .filter(|e| e.end_ms - e.start_ms >= MIN_CUE_MS)
        .filter(|e| !e.positioned)
        .filter(|e| {
            !script
                .styles
                .get(e.style)
                .is_some_and(|s| is_sign_style(&s.name))
        })
        .filter(|e| !has_music_note(&e.text))
        .filter_map(|e| {
            let text = clean_text(&e.text);
            (alignable_words(&text) >= MIN_WORDS).then_some(Eligible {
                start_ms: e.start_ms,
                end_ms: e.end_ms,
                text,
                unique: true,
            })
        })
        .collect();
    cues.sort_by_key(|c| c.start_ms);
    let keys: Vec<String> = cues.iter().map(|c| key(&c.text)).collect();
    for (i, cue) in cues.iter_mut().enumerate() {
        let k = keys.get(i).map_or("", String::as_str);
        cue.unique = keys.iter().filter(|o| o.as_str() == k).count() == 1;
    }
    cues
}

/// FR-004 / FR-005: picks up to [`MAX_PROBES`] probes spread evenly over the
/// track's span, each with a decoy.
pub fn pick_probes(script: &Script) -> Result<Vec<Probe>, TooFewLines> {
    let all = eligible(script);
    let unique: Vec<&Eligible> = all.iter().filter(|c| c.unique).collect();
    if unique.len() < MIN_PROBES {
        return Err(TooFewLines(unique.len()));
    }
    let (Some(first), Some(last)) = (unique.first(), unique.last()) else {
        return Err(TooFewLines(0));
    };
    let (lo, span) = (first.start_ms, last.start_ms - first.start_ms);
    let mut chosen: Vec<usize> = Vec::with_capacity(MAX_PROBES);
    if unique.len() <= MAX_PROBES {
        chosen.extend(0..unique.len());
    } else {
        // The cue nearest each of MAX_PROBES evenly spaced targets, ends included.
        for k in 0..MAX_PROBES {
            let target = lo + span * k as i64 / (MAX_PROBES as i64 - 1);
            let best = (0..unique.len())
                .filter(|i| !chosen.contains(i))
                .min_by_key(|&i| {
                    unique
                        .get(i)
                        .map_or(i64::MAX, |c| (c.start_ms - target).abs())
                });
            if let Some(i) = best {
                chosen.push(i);
            }
        }
        chosen.sort_unstable();
    }
    Ok(chosen
        .into_iter()
        .filter_map(|i| unique.get(i))
        .map(|p| Probe {
            start_ms: p.start_ms,
            end_ms: p.end_ms,
            text: p.text.clone(),
            decoy: decoy_for(p, &all),
        })
        .collect())
}

/// FR-005: the eligible cue nearest [`DECOY_DISTANCE_MS`] away with other text,
/// or the farthest one when none is that far.
fn decoy_for(probe: &Eligible, all: &[Eligible]) -> String {
    let own = key(&probe.text);
    let others = all.iter().filter(|c| key(&c.text) != own);
    let dist = |c: &Eligible| (c.start_ms - probe.start_ms).abs();
    others
        .clone()
        .filter(|c| dist(c) >= DECOY_DISTANCE_MS)
        .min_by_key(|c| dist(c))
        .or_else(|| others.max_by_key(|c| dist(c)))
        .map_or_else(String::new, |c| c.text.clone())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn srt(cues: &[(i64, i64, &str)]) -> Script {
        let mut s = String::new();
        for (i, (a, b, t)) in cues.iter().enumerate() {
            let ts = |ms: i64| {
                format!(
                    "{:02}:{:02}:{:02},{:03}",
                    ms / 3_600_000,
                    ms / 60_000 % 60,
                    ms / 1000 % 60,
                    ms % 1000
                )
            };
            s.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, ts(*a), ts(*b), t));
        }
        Script::parse(&s)
    }

    /// `n` distinct spoken cues, one a minute.
    fn track(n: i64) -> Vec<(i64, i64, String)> {
        (0..n)
            .map(|i| {
                (
                    i * 60_000,
                    i * 60_000 + 2_000,
                    format!("line number {i} is spoken here"),
                )
            })
            .collect()
    }

    fn script_of(cues: &[(i64, i64, String)]) -> Script {
        let refs: Vec<(i64, i64, &str)> =
            cues.iter().map(|(a, b, t)| (*a, *b, t.as_str())).collect();
        srt(&refs)
    }

    #[test]
    fn sdh_tags_and_speaker_labels_are_removed() {
        assert_eq!(
            clean_text("[door slams]\nJOHN: Get out of here"),
            "Get out of here"
        );
        assert_eq!(clean_text("(laughs)\n- Where were you?"), "Where were you?");
        assert_eq!(clean_text("I said (quietly) no"), "I said (quietly) no");
        assert_eq!(clean_text("Time: half past"), "Time: half past");
        assert_eq!(clean_text("MAN 2: Over here"), "Over here");
    }

    #[test]
    fn long_track_gets_twelve_probes_spread_over_its_span() {
        let cues = track(60);
        let probes = pick_probes(&script_of(&cues)).unwrap();
        assert_eq!(probes.len(), MAX_PROBES);
        assert_eq!(probes.first().unwrap().start_ms, 0);
        assert_eq!(probes.last().unwrap().start_ms, 59 * 60_000);
        // No two probes closer than a third of the even spacing.
        let gap = 59 * 60_000 / 11;
        for w in probes.windows(2) {
            assert!(w[1].start_ms - w[0].start_ms > gap / 3, "{w:?}");
        }
    }

    #[test]
    fn decoys_come_from_ten_minutes_away_and_never_repeat_the_probe() {
        let probes = pick_probes(&script_of(&track(60))).unwrap();
        for p in &probes {
            assert_ne!(p.decoy, p.text);
            let n: i64 = p.decoy.split_whitespace().nth(2).unwrap().parse().unwrap();
            assert!(
                (n * 60_000 - p.start_ms).abs() >= DECOY_DISTANCE_MS,
                "{p:?}"
            );
        }
    }

    #[test]
    fn short_file_uses_the_farthest_cue_as_decoy() {
        let probes = pick_probes(&script_of(&track(8))).unwrap();
        assert_eq!(probes.len(), 8);
        assert_eq!(probes[0].decoy, "line number 7 is spoken here");
        assert_eq!(probes[7].decoy, "line number 0 is spoken here");
    }

    #[test]
    fn music_short_repeated_and_wordless_cues_are_not_probes() {
        let mut cues = track(5);
        cues.push((400_000, 403_000, "♪ la la la la ♪".into()));
        cues.push((410_000, 410_400, "far too short to score".into()));
        cues.push((430_000, 433_000, "[music]".into()));
        cues.push((440_000, 443_000, "say it again please".into()));
        cues.push((450_000, 453_000, "Say it again, please!".into()));
        cues.push((460_000, 463_000, "only two".into()));
        let err = pick_probes(&script_of(&cues)).unwrap_err();
        // Only the five spoken lines are left; the repeated pair is dropped.
        assert_eq!(err, TooFewLines(5));
        assert_eq!(err.to_string(), "too_few_lines:5");
    }

    #[test]
    fn signs_by_position_or_style_are_not_probes() {
        let mut src = String::from(
            "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\n\
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1\n\
Style: Signs-Top,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1\n\
\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        );
        for i in 0..6 {
            src.push_str(&format!(
                "Dialogue: 0,0:0{i}:00.00,0:0{i}:03.00,Default,,0,0,0,,spoken line {i} for the test\n"
            ));
        }
        src.push_str(
            "Dialogue: 0,0:07:00.00,0:07:03.00,Signs-Top,,0,0,0,,the shop sign says open\n",
        );
        src.push_str("Dialogue: 0,0:08:00.00,0:08:03.00,Default,,0,0,0,,{\\pos(5,5)}the station board sign\n");
        let probes = pick_probes(&Script::parse(&src)).unwrap();
        assert_eq!(probes.len(), 6);
        assert!(probes.iter().all(|p| p.text.starts_with("spoken line")));
        assert!(probes.iter().all(|p| p.decoy.starts_with("spoken line")));
    }
}
