//! Forgiving matching for spoken search: strip the voice command, then score
//! a candidate against what the recogniser heard.
//!
//! Speech recognition returns words that sound right and are spelled wrong
//! ("cast tone" for "test tone"), so plain text search misses. The score
//! blends spelling similarity (Jaro-Winkler and normalised Levenshtein) with
//! sound similarity (a small Metaphone-style key). Pure and std-only.

/// Words that carry the command, not the title.
const STOPWORDS: &[&str] = &[
    "play",
    "put",
    "on",
    "by",
    "the",
    "a",
    "an",
    "some",
    "my",
    "me",
    "song",
    "songs",
    "track",
    "tracks",
    "album",
    "albums",
    "artist",
    "artists",
    "music",
    "from",
    "of",
    "to",
    "listen",
    "pharos",
    "lighthouse",
    "app",
    "android",
    "auto",
    "please",
    "called",
    "named",
];

/// Phrases that may open a request, longest first.
const LEADING: &[&[&str]] = &[
    &["listen", "to"],
    &["put", "on"],
    &["play", "me"],
    &["play"],
    &["start"],
    &["shuffle"],
];

/// Words that may follow the verb and name the kind wanted.
const KIND_WORDS: &[&str] = &[
    "song", "songs", "track", "tracks", "album", "albums", "artist", "artists", "music",
];

/// What a trailing "on <app>" may name.
const APP_WORDS: &[&str] = &["pharos", "lighthouse", "android", "auto", "the", "app"];

/// Lowercase letters, digits and single spaces; everything else is a break.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if c == '\'' {
            // "don't" -> "dont", not "don t".
        } else if !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    out.trim_end().to_owned()
}

/// The request without its leading voice verb and trailing "on <app>".
/// Empty when nothing but the command was said.
pub fn strip_command(query: &str) -> String {
    let norm = normalize(query);
    let mut words: Vec<&str> = norm.split(' ').filter(|w| !w.is_empty()).collect();

    // Trailing " on|in|with|via|using <app words>".
    if let Some(at) = words
        .iter()
        .rposition(|w| matches!(*w, "on" | "in" | "with" | "via" | "using"))
    {
        let tail = &words[at + 1..];
        if !tail.is_empty()
            && tail.iter().all(|w| APP_WORDS.contains(w))
            && tail.iter().any(|w| *w != "the" && *w != "app")
        {
            words.truncate(at);
        }
    }

    // Leading verb, optional article, optional kind word.
    if let Some(verb) = LEADING.iter().find(|v| words.starts_with(v)) {
        words.drain(..verb.len());
        while words
            .first()
            .is_some_and(|w| matches!(*w, "the" | "a" | "an" | "some" | "my"))
        {
            words.remove(0);
        }
        if words.len() > 1 && words.first().is_some_and(|w| KIND_WORDS.contains(w)) {
            words.remove(0);
        }
        if words.len() > 1
            && words
                .first()
                .is_some_and(|w| matches!(*w, "called" | "named"))
        {
            words.remove(0);
        }
    }
    words.join(" ")
}

/// The words worth a search of their own, longest first, no repeats.
pub fn significant_words(query: &str) -> Vec<String> {
    let mut words: Vec<String> = normalize(query)
        .split(' ')
        .filter(|w| w.chars().count() >= 2 && !STOPWORDS.contains(w))
        .map(str::to_owned)
        .collect();
    words.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
    words.dedup();
    let mut seen = Vec::new();
    words.retain(|w| {
        let new = !seen.contains(w);
        seen.push(w.clone());
        new
    });
    words
}

/// Jaro similarity, 0.0 to 1.0.
fn jaro(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut a_hit = vec![false; a.len()];
    let mut b_hit = vec![false; b.len()];
    let mut matches = 0usize;
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_hit[j] && b[j] == *ca {
                a_hit[i] = true;
                b_hit[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let mut swaps = 0usize;
    let mut j = 0usize;
    for (i, ca) in a.iter().enumerate() {
        if a_hit[i] {
            while !b_hit[j] {
                j += 1;
            }
            if *ca != b[j] {
                swaps += 1;
            }
            j += 1;
        }
    }
    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - swaps as f64 / 2.0) / m) / 3.0
}

/// Jaro-Winkler similarity, 0.0 to 1.0: Jaro plus a bonus for a shared start.
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let j = jaro(&a, &b);
    let prefix = a.iter().zip(&b).take(4).take_while(|(x, y)| x == y).count();
    j + prefix as f64 * 0.1 * (1.0 - j)
}

/// Levenshtein edit distance.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// 1.0 for equal strings, falling to 0.0 as the edit distance reaches the
/// longer length.
pub fn lev_similarity(a: &str, b: &str) -> f64 {
    let longest = a.chars().count().max(b.chars().count());
    if longest == 0 {
        return 1.0;
    }
    1.0 - levenshtein(a, b) as f64 / longest as f64
}

/// A small Metaphone-style sound key of one lowercase word: consonants only,
/// with the common English spellings folded together.
fn metaphone(word: &str) -> String {
    let w: Vec<char> = word.chars().filter(|c| c.is_alphanumeric()).collect();
    let is_vowel = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u');
    let at = |i: usize| w.get(i).copied().unwrap_or('\0');
    let mut out = String::new();
    let mut i = 0;
    // Silent starts.
    if matches!(
        (at(0), at(1)),
        ('k', 'n') | ('g', 'n') | ('p', 'n') | ('w', 'r')
    ) {
        i = 1;
    } else if at(0) == 'x' {
        out.push('S');
        i = 1;
    } else if (at(0), at(1)) == ('w', 'h') {
        out.push('W');
        i = 2;
    }
    while i < w.len() {
        let c = w[i];
        let next = at(i + 1);
        if c == at(i.wrapping_sub(1)) && c != 'c' && i > 0 {
            i += 1;
            continue;
        }
        let mut skip = 0;
        let code: Option<&str> = match c {
            'a' | 'e' | 'i' | 'o' | 'u' => {
                if i == 0 {
                    Some("A")
                } else {
                    None
                }
            }
            'b' => (!(i + 1 == w.len() && at(i.wrapping_sub(1)) == 'm')).then_some("B"),
            'c' => {
                if next == 'h' {
                    skip = 1;
                    Some("X")
                } else if matches!(next, 'e' | 'i' | 'y') {
                    Some("S")
                } else {
                    Some("K")
                }
            }
            'd' => {
                if next == 'g' && matches!(at(i + 2), 'e' | 'i' | 'y') {
                    skip = 1;
                    Some("J")
                } else {
                    Some("T")
                }
            }
            'g' => {
                if next == 'h' && !is_vowel(at(i + 2)) {
                    None
                } else if matches!(next, 'e' | 'i' | 'y') {
                    Some("J")
                } else {
                    Some("K")
                }
            }
            'h' => (is_vowel(next)
                && !matches!(at(i.wrapping_sub(1)), 'c' | 's' | 'p' | 't' | 'g'))
            .then_some("H"),
            'k' => (at(i.wrapping_sub(1)) != 'c').then_some("K"),
            'p' => {
                if next == 'h' {
                    skip = 1;
                    Some("F")
                } else {
                    Some("P")
                }
            }
            'q' => Some("K"),
            's' => {
                if next == 'h' {
                    skip = 1;
                    Some("X")
                } else {
                    Some("S")
                }
            }
            't' => {
                if next == 'h' {
                    skip = 1;
                    Some("0")
                } else if next == 'i' && matches!(at(i + 2), 'o' | 'a') {
                    Some("X")
                } else {
                    Some("T")
                }
            }
            'v' => Some("F"),
            'w' | 'y' => is_vowel(next).then_some(if c == 'w' { "W" } else { "Y" }),
            'x' => Some("KS"),
            'z' => Some("S"),
            'f' => Some("F"),
            'j' => Some("J"),
            'l' => Some("L"),
            'm' => Some("M"),
            'n' => Some("N"),
            'r' => Some("R"),
            d if d.is_ascii_digit() => None,
            _ => None,
        };
        if let Some(code) = code {
            out.push_str(code);
        } else if c.is_ascii_digit() {
            out.push(c);
        }
        i += 1 + skip;
    }
    out
}

/// The sound key of a phrase with the spaces removed, so "bee yon say" and
/// "beyonce" meet. Words are keyed one at a time and joined.
pub fn phonetic_key(text: &str) -> String {
    normalize(text).split(' ').map(metaphone).collect()
}

/// How alike two phrases read and sound, 0.0 to 1.0.
pub fn similarity(heard: &str, candidate: &str) -> f64 {
    let a = normalize(heard);
    let b = normalize(candidate);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let spelling = 0.5 * jaro_winkler(&a, &b) + 0.5 * lev_similarity(&a, &b);
    let (ka, kb) = (phonetic_key(&a), phonetic_key(&b));
    let sound = if ka.is_empty() || kb.is_empty() {
        0.0
    } else {
        0.5 * jaro_winkler(&ka, &kb) + 0.5 * lev_similarity(&ka, &kb)
    };
    0.5 * spelling + 0.5 * sound
}

/// The best [`similarity`] of the request to a title, an artist, or both
/// together: people ask for any of the three.
pub fn best_similarity(heard: &str, title: &str, artist: &str) -> f64 {
    let mut best = similarity(heard, title);
    if !artist.is_empty() {
        best = best
            .max(similarity(heard, artist))
            .max(similarity(heard, &format!("{title} {artist}")))
            .max(similarity(heard, &format!("{artist} {title}")));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_case_punctuation_and_apostrophes() {
        assert_eq!(normalize("  Don't  Stop!! (Live) "), "dont stop live");
    }

    #[test]
    fn strips_leading_verbs_and_trailing_app_names() {
        for (said, want) in [
            ("play abbey road on pharos", "abbey road"),
            ("Play the album Abbey Road", "abbey road"),
            ("put on test tone", "test tone"),
            ("play the song blue lamp on the lighthouse app", "blue lamp"),
            ("listen to red kettle in android auto", "red kettle"),
            ("test tone c", "test tone c"),
            ("turn on", "turn on"),
            ("play", ""),
            ("play some music", "music"),
        ] {
            assert_eq!(strip_command(said), want, "{said}");
        }
    }

    #[test]
    fn a_title_that_contains_on_is_kept() {
        assert_eq!(strip_command("play dancing on ice"), "dancing on ice");
    }

    #[test]
    fn significant_words_drop_stopwords_longest_first() {
        assert_eq!(
            significant_words("play the song cast tone c on pharos"),
            ["cast", "tone"]
        );
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert!((lev_similarity("abc", "abc") - 1.0).abs() < 1e-9);
        assert!(jaro_winkler("martha", "marhta") > 0.95);
        assert!(jaro_winkler("abc", "xyz") < 0.1);
    }

    #[test]
    fn homophones_share_a_key() {
        assert_eq!(phonetic_key("bee yon say"), phonetic_key("beyonce"));
        assert_eq!(phonetic_key("knight"), phonetic_key("night"));
        assert_eq!(phonetic_key("phone"), phonetic_key("fone"));
    }

    #[test]
    fn misheard_scores_above_unrelated() {
        let near = similarity("cast tone c", "Test tone C");
        let far = similarity("cast tone c", "Sunrise over the quarry");
        assert!(near > 0.8 && far < 0.5, "near {near} far {far}");
    }
}
