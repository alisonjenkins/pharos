use pharos_align::text::words;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Fixture {
    unicode_version: String,
    unassigned: Vec<(u32, u32)>,
    spaces: Vec<u32>,
    single: BTreeMap<String, String>,
    strings: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    text: String,
    words: Vec<(usize, String)>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../fixtures/text.json")).unwrap()
}

fn all_chars() -> impl Iterator<Item = char> {
    (0..0x11_0000u32).filter_map(char::from_u32)
}

#[test]
fn whitespace_splits_on_exactly_pythons_set() {
    let f = fixture();
    let spaces: std::collections::BTreeSet<u32> = f.spaces.iter().copied().collect();
    for c in all_chars() {
        // A lone space between two letters splits them only if Python's `\s` matches it.
        let split = words(&format!("a{c}b")).len() == 2;
        assert_eq!(
            split,
            spaces.contains(&(c as u32)),
            "U+{:04X} (python unicode {})",
            c as u32,
            f.unicode_version
        );
    }
}

#[test]
fn every_single_character_folds_like_python() {
    let f = fixture();
    let mut mismatches = Vec::new();
    let assigned = |c: char| {
        let i = f.unassigned.partition_point(|&(_, hi)| hi < c as u32);
        f.unassigned.get(i).is_none_or(|&(lo, _)| (c as u32) < lo)
    };
    for c in all_chars().filter(|&c| assigned(c)) {
        let got = words(&c.to_string()).into_iter().next().map(|w| w.token);
        let want = f.single.get(&(c as u32).to_string()).cloned();
        if got != want {
            mismatches.push(format!("U+{:04X}: rust {got:?} python {want:?}", c as u32));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} chars differ (python unicode {}):\n{}",
        mismatches.len(),
        f.unicode_version,
        mismatches.join("\n")
    );
}

#[test]
fn strings_match_python_words_and_offsets() {
    for case in fixture().strings {
        let got: Vec<(usize, String)> = words(&case.text)
            .into_iter()
            .map(|w| (w.at, w.token))
            .collect();
        assert_eq!(got, case.words, "{:?}", case.text);
    }
}
