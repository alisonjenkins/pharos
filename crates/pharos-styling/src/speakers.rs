//! Palette slots for the people a script names as speakers (speaker
//! colours, opt-in).

use std::collections::HashMap;

use crate::source::SourceLine;

/// Fewest dialogue lines a speaker needs for a colour: a name seen once or
/// twice is a walk-on, and a colour for it tells the viewer nothing.
pub const MIN_SPEAKER_LINES: usize = 3;

/// Most speakers coloured: past this the colours stop being told apart.
pub const MAX_SPEAKERS: usize = 6;

/// Names scripts put in the actor field that are not a person speaking.
const NOT_SPEAKERS: [&str; 11] = [
    "sign", "signs", "tl note", "tl", "note", "op", "ed", "song", "lyrics", "karaoke", "title",
];

/// One slot per line: the [`MAX_SPEAKERS`] speakers with the most dialogue
/// lines (at least [`MIN_SPEAKER_LINES`]) get slots 0 up, most lines first,
/// ties to whoever spoke first. Typeset lines, lines with no speaker and
/// names that are not one person get none.
pub fn slots(lines: &[SourceLine]) -> Vec<Option<u8>> {
    let keys: Vec<Option<String>> = lines.iter().map(key).collect();
    let mut tally: HashMap<&str, (usize, usize)> = HashMap::new();
    for (i, k) in keys.iter().enumerate() {
        if let Some(k) = k {
            tally.entry(k).or_insert((0, i)).0 += 1;
        }
    }
    let mut ranked: Vec<(&str, usize, usize)> = tally
        .into_iter()
        .filter(|(_, (n, _))| *n >= MIN_SPEAKER_LINES)
        .map(|(k, (n, first))| (k, n, first))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    let slot: HashMap<&str, u8> = ranked
        .iter()
        .take(MAX_SPEAKERS)
        .zip(0u8..)
        .map(|((k, _, _), s)| (*k, s))
        .collect();
    keys.iter()
        .map(|k| k.as_deref().and_then(|k| slot.get(k).copied()))
        .collect()
}

/// The speaker a line counts toward, folded so "Kai" and "kai " match.
fn key(line: &SourceLine) -> Option<String> {
    if line.typeset {
        return None;
    }
    let name = line.speaker.as_deref()?.trim().to_lowercase();
    let several =
        name.contains(['/', '&', ',', '+']) || name.split_whitespace().any(|w| w == "and");
    if name.is_empty() || several || NOT_SPEAKERS.contains(&name.as_str()) {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::effects::testutil;

    fn said(by: &[&str]) -> Vec<SourceLine> {
        by.iter()
            .map(|s| SourceLine {
                speaker: (!s.is_empty()).then(|| (*s).to_owned()),
                ..testutil::line("Hello")
            })
            .collect()
    }

    #[test]
    fn the_busiest_speakers_get_the_first_slots() {
        let lines = said(&["Bo", "Ann", "Ann", "Bo", "Ann", "Bo", "Bo", "Cy", "Cy"]);
        let s = slots(&lines);
        // Bo has 4 lines, Ann 3, Cy only 2.
        assert_eq!(
            s,
            [
                Some(0),
                Some(1),
                Some(1),
                Some(0),
                Some(1),
                Some(0),
                Some(0),
                None,
                None
            ]
        );
    }

    #[test]
    fn ties_go_to_whoever_spoke_first_and_case_is_folded() {
        let lines = said(&["Ann", "bo", "ann ", "Bo", "ANN", "BO"]);
        assert_eq!(
            slots(&lines),
            [Some(0), Some(1), Some(0), Some(1), Some(0), Some(1)]
        );
    }

    #[test]
    fn signs_groups_and_typeset_lines_get_no_slot() {
        let mut lines = said(&[
            "Sign", "Sign", "Sign", "Ann/Bo", "Ann/Bo", "Ann/Bo", "", "", "",
        ]);
        lines.extend(said(&["Ann", "Ann", "Ann"]));
        lines[11].typeset = true;
        let s = slots(&lines);
        assert!(s[..9].iter().all(Option::is_none), "{s:?}");
        // Two dialogue lines are not enough once the typeset one is dropped.
        assert_eq!(&s[9..], [None, None, None]);
    }

    #[test]
    fn only_the_busiest_six_are_coloured() {
        let names = ["A", "B", "C", "D", "E", "F", "G"];
        let mut by = Vec::new();
        for (i, n) in names.iter().enumerate() {
            // A speaks 10 lines, B 9, ... G 4.
            by.extend(std::iter::repeat_n(*n, 10 - i));
        }
        let s = slots(&said(&by));
        assert_eq!(s.iter().flatten().max(), Some(&5));
        assert_eq!(s.last(), Some(&None));
    }
}
