//! Another online lyric for a song whose found one was not what was sung
//! (B1479, V1468): the provider's other records for the song, scored
//! against what the aligner heard by the measure the aligner judged the
//! first one with.

use pharos_core::lyrics_text::bigram_recall;

use super::Lyrics;

/// How far a record's length may be from the song's: LRCLIB stores whole
/// seconds and a rip's length can differ from the release's by a second,
/// while another cut of the song is longer than this.
pub const DURATION_WINDOW_MS: u64 = 2_000;
/// The most records scored for one song, closest in length first.
pub const MAX_CANDIDATES: usize = 5;
/// The least text recall a record needs to be picked: the right lyric
/// measured 0.917 against a transcript the wrong one scored 0.107 on, and a
/// record that is only partly right is worse than none.
pub const MIN_RECALL: f64 = 0.6;

/// One record a provider holds for a song.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The provider's id for the record.
    pub id: u64,
    pub duration_ms: u64,
    pub lyrics: Lyrics,
}

/// The record picked and its text recall against what was heard.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub candidate: Candidate,
    pub recall: f64,
}

/// The best of `candidates` for a song `duration_ms` long, judged against
/// `heard`: within [`DURATION_WINDOW_MS`], not the text of the lyric that
/// failed (`current`), the [`MAX_CANDIDATES`] closest in length, and only
/// one whose recall reaches [`MIN_RECALL`]. The higher recall wins, and
/// on a tie the record closer in length.
pub fn choose(
    candidates: Vec<Candidate>,
    current: &Lyrics,
    duration_ms: u64,
    heard: &str,
) -> Option<Pick> {
    let mut near: Vec<Candidate> = candidates
        .into_iter()
        .filter(|c| c.duration_ms.abs_diff(duration_ms) <= DURATION_WINDOW_MS)
        .filter(|c| !same_text(&c.lyrics, current))
        .collect();
    near.sort_by_key(|c| c.duration_ms.abs_diff(duration_ms));
    near.truncate(MAX_CANDIDATES);
    let mut best: Option<Pick> = None;
    for candidate in near {
        let Some(recall) = bigram_recall(
            candidate.lyrics.lines.iter().map(|l| l.text.as_str()),
            heard,
        ) else {
            continue;
        };
        if recall >= MIN_RECALL && best.as_ref().map_or(true, |b| recall > b.recall) {
            best = Some(Pick { candidate, recall });
        }
    }
    best
}

/// The record that failed comes back from a search too, and so may a copy
/// of it under another id; either fails the same way.
fn same_text(a: &Lyrics, b: &Lyrics) -> bool {
    a.lines.len() == b.lines.len()
        && a.lines
            .iter()
            .zip(&b.lines)
            .all(|(x, y)| x.text.trim() == y.text.trim())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::{LyricLine, Origin};
    use super::*;

    const SONG_MS: u64 = 200_000;
    const HEARD: &str = "grey gulls wheel above the quay low tide hums along the stones";

    fn lyrics(lines: &[&str]) -> Lyrics {
        Lyrics::from_lines(
            lines
                .iter()
                .enumerate()
                .map(|(i, t)| LyricLine {
                    start_ms: Some(i as u64 * 4_000),
                    text: (*t).into(),
                    words: Vec::new(),
                })
                .collect(),
            Origin::Online,
        )
        .unwrap()
    }

    fn sung() -> Lyrics {
        lyrics(&[
            "grey gulls wheel above the quay",
            "low tide hums along the stones",
        ])
    }

    fn wrong() -> Lyrics {
        lyrics(&[
            "the lanterns swing in the orchard",
            "the river keeps its time",
        ])
    }

    fn candidate(id: u64, duration_ms: u64, lyrics: Lyrics) -> Candidate {
        Candidate {
            id,
            duration_ms,
            lyrics,
        }
    }

    #[test]
    fn the_record_that_was_sung_is_picked() {
        let got = choose(
            vec![
                candidate(1, SONG_MS, wrong()),
                candidate(2, SONG_MS + 1_000, sung()),
            ],
            &wrong(),
            SONG_MS,
            HEARD,
        )
        .unwrap();
        assert_eq!(got.candidate.id, 2);
        assert_eq!(got.recall, 1.0);
    }

    #[test]
    fn a_record_of_another_length_is_not_scored() {
        let got = choose(
            vec![candidate(2, SONG_MS + DURATION_WINDOW_MS + 1, sung())],
            &wrong(),
            SONG_MS,
            HEARD,
        );
        assert_eq!(got, None);
        let edge = choose(
            vec![candidate(3, SONG_MS - DURATION_WINDOW_MS, sung())],
            &wrong(),
            SONG_MS,
            HEARD,
        );
        assert_eq!(edge.map(|p| p.candidate.id), Some(3));
    }

    #[test]
    fn the_lyric_that_failed_is_not_picked_again_under_any_id() {
        // Even if it were heard: this text was just found not to fit.
        let got = choose(vec![candidate(9, SONG_MS, sung())], &sung(), SONG_MS, HEARD);
        assert_eq!(got, None);
    }

    #[test]
    fn a_record_heard_in_part_is_not_picked() {
        let half = lyrics(&[
            "grey gulls wheel above the quay",
            "the river keeps its time",
        ]);
        let got = choose(vec![candidate(4, SONG_MS, half)], &wrong(), SONG_MS, HEARD);
        assert_eq!(got, None);
    }

    #[test]
    fn only_the_five_closest_in_length_are_scored() {
        let mut all: Vec<Candidate> = (0..MAX_CANDIDATES as u64)
            .map(|i| candidate(10 + i, SONG_MS + i * 100, wrong_but_new(i)))
            .collect();
        all.push(candidate(99, SONG_MS + 1_500, sung()));
        assert_eq!(choose(all.clone(), &wrong(), SONG_MS, HEARD), None);
        all.truncate(MAX_CANDIDATES - 1);
        all.push(candidate(99, SONG_MS + 1_500, sung()));
        let got = choose(all, &wrong(), SONG_MS, HEARD);
        assert_eq!(got.map(|p| p.candidate.id), Some(99));
    }

    fn wrong_but_new(i: u64) -> Lyrics {
        lyrics(&[&format!("lantern number {i} swings"), "the orchard sleeps"])
    }

    #[test]
    fn the_better_heard_record_wins() {
        let most = lyrics(&[
            "grey gulls wheel above the quay",
            "low tide hums along the stones",
            "the river keeps its time",
        ]);
        let got = choose(
            vec![
                candidate(5, SONG_MS, most),
                candidate(6, SONG_MS + 500, sung()),
            ],
            &wrong(),
            SONG_MS,
            HEARD,
        );
        assert_eq!(got.map(|p| p.candidate.id), Some(6));
    }
}
