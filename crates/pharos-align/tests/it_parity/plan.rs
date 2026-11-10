use std::cell::RefCell;
use std::collections::HashMap;

use pharos_align::offset::{estimate, probe_indices, shift};
use pharos_align::plan::{parse_request, stamps, window, Line};
use pharos_align::text::words;
use serde::Deserialize;
use serde_json::Value;

type RawLine = (i64, i64, String);
/// start, end, word count, previous line's end, expected window.
type WindowCase = (i64, i64, usize, i64, (i64, i64));

#[derive(Deserialize)]
struct Fixture {
    windows: Vec<WindowCase>,
    stamps: Vec<StampCase>,
    parse: Vec<ParseCase>,
    offset: Vec<OffsetCase>,
}

#[derive(Deserialize)]
struct StampCase {
    text: String,
    spans: Vec<(u64, u64)>,
    window_start_ms: i64,
    stamps: Vec<(usize, i64)>,
}

#[derive(Deserialize)]
struct ParseCase {
    body: Value,
    path: Option<String>,
    lines: Option<Vec<RawLine>>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct OffsetCase {
    lines: Vec<RawLine>,
    probe_indices: Vec<usize>,
    /// Per probe line: `None` when decode found nothing, else the aligner's spans.
    answers: HashMap<String, Option<Vec<(u64, u64)>>>,
    decode_calls: Vec<(i64, i64)>,
    offset_ms: i64,
    probes: Vec<Option<i64>>,
    reason: String,
    shifted: Vec<RawLine>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../fixtures/plan.json")).unwrap()
}

fn to_lines(raw: &[RawLine]) -> Vec<Line> {
    raw.iter()
        .map(|(s, e, t)| Line {
            start_ms: *s,
            end_ms: *e,
            text: t.clone(),
        })
        .collect()
}

#[test]
fn windows_match_python() {
    for (start, end, n, prev, want) in fixture().windows {
        let line = Line {
            start_ms: start,
            end_ms: end,
            text: String::new(),
        };
        assert_eq!(window(&line, n, prev), want, "{start} {end} {n} {prev}");
    }
}

#[test]
fn stamps_match_python() {
    for case in fixture().stamps {
        let got: Vec<(usize, i64)> = stamps(
            &case.text,
            &words(&case.text),
            &case.spans,
            case.window_start_ms,
        )
        .into_iter()
        .map(|s| (s.at, s.start_ms))
        .collect();
        assert_eq!(got, case.stamps, "{:?} {:?}", case.text, case.spans);
    }
}

#[test]
fn parse_request_matches_python() {
    for case in fixture().parse {
        match (parse_request(&case.body), case.error) {
            (Err(e), Some(want)) => assert_eq!(e.to_string(), want),
            (Ok((path, lines)), None) => {
                assert_eq!(Some(path), case.path);
                assert_eq!(Some(lines), case.lines.as_deref().map(to_lines));
            }
            (got, want) => panic!("{} gave {got:?}, Python {want:?}", case.body),
        }
    }
}

#[test]
fn offset_estimate_matches_python() {
    for case in fixture().offset {
        let lines = to_lines(&case.lines);
        assert_eq!(probe_indices(&lines), case.probe_indices);
        let calls = RefCell::new(Vec::new());
        let decode = |s: i64, e: i64| {
            let mut calls = calls.borrow_mut();
            let probe = case.probe_indices[calls.len()];
            calls.push((s, e));
            // The window carries its probe's line index, for `align` to look up.
            Ok::<_, std::convert::Infallible>(match case.answers[&probe.to_string()] {
                Some(_) => vec![probe as f32],
                None => Vec::new(),
            })
        };
        let align = |windows: &[(&[f32], &[String])]| {
            assert_eq!(
                calls.borrow().len(),
                case.probe_indices.len(),
                "all decoded first"
            );
            Ok(windows
                .iter()
                .map(|(pcm, _)| {
                    let probe = pcm[0] as usize;
                    case.answers[&probe.to_string()].clone().unwrap()
                })
                .collect())
        };
        let Ok(got) = estimate(&lines, decode, align);
        assert_eq!(calls.into_inner(), case.decode_calls);
        assert_eq!(got.probes, case.probes, "{}", case.reason);
        assert_eq!(
            (got.offset_ms, got.reason.as_str()),
            (case.offset_ms, case.reason.as_str())
        );
        assert_eq!(shift(lines, got.offset_ms), to_lines(&case.shifted));
    }
}
