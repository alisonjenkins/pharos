#![allow(clippy::unwrap_used, clippy::float_cmp)]

use metrics_util::debugging::{DebugValue, DebuggingRecorder};

use super::{
    retry_windows, wants_retry, words, Decode, Outcome, Retry, Segment, Token, RETRY_MIN_DISTINCT,
    RETRY_MIN_WORDS,
};
use crate::obs::RECOGNISE_RETRY_TOTAL;

fn tok(text: &str, t0_s: f64, dtw_s: f64) -> Token {
    Token {
        text: text.to_owned(),
        prob: 0.5,
        t0_s,
        dtw_s,
    }
}

fn seg(start_s: f64, end_s: f64, tokens: Vec<Token>) -> Segment {
    Segment {
        start_s,
        end_s,
        text: tokens.iter().map(|t| t.text.as_str()).collect(),
        tokens,
    }
}

/// `n` different one-token words, one a second from `from_s`.
fn passage(from_s: f64, n: usize) -> Vec<Token> {
    (0..n)
        .map(|i| {
            let s = from_s + i as f64;
            tok(&format!(" w{i}"), s, s + 0.8)
        })
        .collect()
}

/// `n` one-token words, one phrase over and over.
fn chant(from_s: f64, n: usize) -> Vec<Token> {
    (0..n)
        .map(|i| {
            let s = from_s + i as f64;
            tok(if i % 2 == 0 { " It's" } else { " like," }, s, s + 0.8)
        })
        .collect()
}

#[test]
fn a_word_starts_where_the_word_before_it_ends() {
    let w = words(&[
        tok(" Hel", 1.0, 1.4),
        tok("lo", 1.2, 1.7),
        tok(" there", 1.8, 2.5),
    ]);
    assert_eq!(w.len(), 2);
    assert_eq!(w[0].text, " Hello");
    assert_eq!((w[0].start_s, w[0].end_s), (1.0, 1.7));
    // Its own first DTW time (2.5) is where it ends, not where it starts.
    assert_eq!((w[1].start_s, w[1].end_s), (1.7, 2.5));
}

#[test]
fn a_word_never_ends_before_it_starts() {
    let w = words(&[tok(" a", 1.0, 2.0), tok(" b", 1.5, 1.9)]);
    assert_eq!((w[1].start_s, w[1].end_s), (2.0, 2.0));
}

#[test]
fn a_word_carries_its_tokens_mean_probability() {
    let mut a = tok(" ab", 0.0, 0.5);
    a.prob = 0.2;
    let mut b = tok("c", 0.1, 0.6);
    b.prob = 0.6;
    let w = words(&[a, b]);
    assert!((w[0].prob - 0.4).abs() < 1e-6);
}

#[test]
fn blank_tokens_make_no_word() {
    let w = words(&[tok(" ", 0.0, 0.1), tok(" la", 0.2, 0.5)]);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].text, " la");
}

#[test]
fn a_caption_over_a_window_wants_a_retry() {
    let caption = seg(
        140.0,
        170.0,
        vec![tok(" I'm", 140.0, 141.0), tok(" sorry.", 141.0, 142.0)],
    );
    assert!(wants_retry(&caption));
}

#[test]
fn a_short_segment_or_a_long_passage_does_not() {
    let short = seg(
        140.0,
        160.0,
        vec![tok(" Thank", 140.0, 141.0), tok(" you.", 141.0, 142.0)],
    );
    assert!(!wants_retry(&short));
    let sung = seg(140.0, 170.0, passage(140.0, 4));
    assert!(!wants_retry(&sung));
}

#[test]
fn the_first_decode_keeps_whispers_defaults_but_two() {
    // Under whisper.cpp's default of 2.4, which dropped a sung chorus.
    assert_eq!(Decode::FIRST.entropy_thold, 2.3);
    assert_eq!(Decode::FIRST.n_max_text_ctx, 0);
    assert_eq!(Decode::FIRST.beam, None);
    assert_eq!(Decode::RETRY.beam, Some(5));
    assert_eq!(Decode::RETRY.entropy_thold, Decode::FIRST.entropy_thold);
}

#[test]
fn a_retry_that_hears_a_passage_replaces_the_window() {
    let first = vec![
        seg(0.0, 10.0, passage(0.0, 5)),
        seg(
            10.0,
            40.0,
            vec![tok(" Thank", 10.0, 11.0), tok(" you.", 11.0, 12.0)],
        ),
        seg(40.0, 45.0, passage(40.0, 3)),
    ];
    let mut asked = Vec::new();
    let (out, retries) = retry_windows(first, 200.0, |s, e| {
        asked.push((s, e));
        // Two words past the caption's end are the next segment's to keep.
        Ok::<_, ()>(vec![seg(10.0, 42.0, passage(10.0, RETRY_MIN_WORDS + 2))])
    })
    .unwrap();
    assert_eq!(asked, vec![(10.0, 40.0)]);
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].outcome, Outcome::Replaced);
    assert_eq!(retries[0].first, "Thank you.");
    assert_eq!(out.len(), 3);
    assert!(out[1].tokens.iter().all(|t| t.dtw_s <= 40.0));
    assert_eq!(out[1].end_s, 40.0);
    assert_eq!(out[2].start_s, 40.0);
}

#[test]
fn a_retry_that_hears_another_caption_keeps_the_first_decode() {
    let caption = seg(
        10.0,
        40.0,
        vec![tok(" Thank", 10.0, 11.0), tok(" you.", 11.0, 12.0)],
    );
    let (out, retries) = retry_windows(vec![caption.clone()], 200.0, |_, _| {
        Ok::<_, ()>(vec![seg(10.0, 40.0, passage(10.0, RETRY_MIN_WORDS - 1))])
    })
    .unwrap();
    assert_eq!(retries[0].outcome, Outcome::Kept);
    assert_eq!(retries[0].words, RETRY_MIN_WORDS - 1);
    assert_eq!(out, vec![caption]);
}

#[test]
fn a_retry_that_loops_keeps_the_first_decode() {
    let caption = seg(
        0.0,
        30.0,
        vec![tok(" Thank", 0.0, 1.0), tok(" you.", 1.0, 2.0)],
    );
    let (out, retries) = retry_windows(vec![caption.clone()], 200.0, |_, _| {
        Ok::<_, ()>(vec![seg(0.0, 30.0, chant(0.0, 2 * RETRY_MIN_WORDS))])
    })
    .unwrap();
    assert_eq!(retries[0].outcome, Outcome::Looped);
    // "It's" and "like," are the only two words, case and commas aside.
    assert!((retries[0].distinct - 2.0 / (2 * RETRY_MIN_WORDS) as f64).abs() < 1e-9);
    assert!(retries[0].distinct < RETRY_MIN_DISTINCT);
    assert_eq!(out, vec![caption]);
}

#[test]
fn a_retry_window_stops_at_the_end_of_the_song() {
    let caption = seg(180.0, 210.0, vec![tok(" Bye.", 180.0, 181.0)]);
    let mut asked = Vec::new();
    retry_windows(vec![caption], 195.0, |s, e| {
        asked.push((s, e));
        Ok::<_, ()>(Vec::new())
    })
    .unwrap();
    assert_eq!(asked, vec![(180.0, 195.0)]);
}

#[test]
fn songs_without_a_lost_window_are_decoded_once() {
    let first = vec![seg(0.0, 10.0, passage(0.0, 5))];
    let (out, retries) = retry_windows(first.clone(), 200.0, |_, _| -> Result<_, ()> {
        panic!("decoded again")
    })
    .unwrap();
    assert_eq!(out, first);
    assert!(retries.is_empty());
}

#[test]
fn outcome_labels_are_distinct() {
    let mut l: Vec<_> = Outcome::ALL.iter().map(|o| o.label()).collect();
    l.sort_unstable();
    l.dedup();
    assert_eq!(l.len(), Outcome::ALL.len());
}

#[test]
fn a_retry_is_counted_by_outcome() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let _guard = metrics::set_default_local_recorder(&recorder);
    Retry {
        start_s: 10.0,
        end_s: 40.0,
        first: "Thank you.".to_owned(),
        words: 12,
        distinct: 0.7,
        outcome: Outcome::Replaced,
    }
    .record();
    let snapshot = snapshotter.snapshot().into_vec();
    let count = |value: &str| {
        snapshot
            .iter()
            .find(|(k, ..)| {
                k.key().name() == RECOGNISE_RETRY_TOTAL
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == value)
            })
            .map(|(.., v)| v)
    };
    assert_eq!(count("replaced"), Some(&DebugValue::Counter(1)));
    assert_eq!(count("kept"), None);
    assert_eq!(count("looped"), None);
}
