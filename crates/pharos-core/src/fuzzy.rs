//! Token matching for search (B624/V558): a term is split into words, and
//! each word must be found in one of an item's names (title, show, artist,
//! album), in any order and any case, allowing one typo in a word of four
//! letters or more. [`score`] ranks how well a set of names holds a term.

use crate::search_tokens;

/// The most words a search term is matched by one at a time; a longer term
/// (pasted text, say) is matched whole, so a term never grows the query
/// past a few dozen clauses.
pub const MAX_WORDS: usize = 12;

/// The shortest search word that may carry a typo: a one-letter slip in
/// "go" or "the" matches far too much to be a correction.
pub const TYPO_MIN_CHARS: usize = 4;

/// How well one search word is held by a name's words, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WordMatch {
    /// The whole word.
    Exact,
    /// The start of a word, as "mob" holds "Moby".
    Prefix,
    /// Inside a word, as "kemon" in "Pokemon".
    Inside,
    /// One edit (a letter added, dropped, changed or two swapped) away from a
    /// word or the start of one.
    Typo,
}

impl WordMatch {
    /// The `reason` label of `pharos_search_match_total`.
    pub fn label(self) -> &'static str {
        match self {
            WordMatch::Exact => "exact",
            WordMatch::Prefix => "prefix",
            WordMatch::Inside => "inside",
            WordMatch::Typo => "typo",
        }
    }

    fn cost(self) -> u32 {
        match self {
            WordMatch::Exact => 0,
            WordMatch::Prefix => 1,
            WordMatch::Inside => 2,
            WordMatch::Typo => 4,
        }
    }
}

/// How well `word` (already folded) is held by `words`, or `None`.
pub fn word_match(word: &str, words: &[String]) -> Option<WordMatch> {
    let mut best: Option<WordMatch> = None;
    let typo_ok = word.chars().count() >= TYPO_MIN_CHARS;
    for w in words {
        let found = if w == word {
            Some(WordMatch::Exact)
        } else if w.starts_with(word) {
            Some(WordMatch::Prefix)
        } else if w.contains(word) {
            Some(WordMatch::Inside)
        } else if typo_ok && one_edit_from_word_or_start(word, w) {
            Some(WordMatch::Typo)
        } else {
            None
        };
        best = match (best, found) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if best == Some(WordMatch::Exact) {
            break;
        }
    }
    best
}

/// Whether `word` is one edit from `w` or from a start of `w` (so a typo
/// in a word still being typed counts).
fn one_edit_from_word_or_start(word: &str, w: &str) -> bool {
    let a: Vec<char> = word.chars().collect();
    let b: Vec<char> = w.chars().collect();
    if osa_distance(&a, &b) <= 1 {
        return true;
    }
    // A start of `w` one letter shorter, as long, or one longer than `word`.
    (a.len().saturating_sub(1)..=a.len() + 1)
        .filter(|n| *n >= TYPO_MIN_CHARS && *n < b.len())
        .any(|n| osa_distance(&a, &b[..n]) <= 1)
}

/// The optimal-string-alignment distance: insertions, deletions,
/// substitutions and swaps of two neighbouring letters, each one edit.
pub fn osa_distance(a: &[char], b: &[char]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > 1 && n.min(m) > 0 {
        // More than one apart already; the exact count is never needed.
        return n.abs_diff(m);
    }
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[n][m]
}

/// How well a set of names holds a search term, best (lowest) first: the
/// summed cost of every search word's best match, then whether the first
/// name holds the whole term as typed (its start, then anywhere), then how
/// many of the first name's words the term left unmatched, then that name's
/// length. So "Dragon Ball" leads "Dragon Ball GT", and an album named for
/// both words of "go moby" leads one that only holds them with typos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Score {
    cost: u32,
    phrase: u8,
    unmatched: usize,
    len: usize,
}

impl Score {
    /// Whether the first name is the term itself, any case.
    pub fn is_exact(self) -> bool {
        self.cost == 0 && self.phrase == 0 && self.unmatched == 0
    }

    /// Whether any word only matched through a typo.
    pub fn has_typo(self) -> bool {
        self.cost >= WordMatch::Typo.cost()
    }
}

/// How well `names` (the first is the item's own name) hold `term`, or
/// `None` when any word of the term is in none of them. A term with no
/// words matches nothing.
pub fn score(names: &[&str], term: &str) -> Option<Score> {
    let tokens = search_tokens(term);
    if tokens.is_empty() {
        return None;
    }
    let words: Vec<String> = names.iter().flat_map(|n| search_tokens(n)).collect();
    let own: Vec<String> = names.first().map(|n| search_tokens(n)).unwrap_or_default();
    let mut cost = 0;
    for t in &tokens {
        cost += word_match(t, &words)?.cost();
    }
    let name = names.first().copied().unwrap_or("").to_lowercase();
    let phrase_text = tokens.join(" ");
    let joined = own.join(" ");
    let phrase = if joined == phrase_text {
        0
    } else if joined.starts_with(&phrase_text) {
        1
    } else if joined.contains(&phrase_text) {
        2
    } else {
        3
    };
    let unmatched = own
        .iter()
        .filter(|w| {
            let w = std::slice::from_ref(*w);
            !tokens.iter().any(|t| word_match(t, w).is_some())
        })
        .count();
    Some(Score {
        cost,
        phrase,
        unmatched,
        len: name.chars().count(),
    })
}

/// A `LIKE` needle of the loose pass: `head`, then `gap` letters of any
/// kind, then `tail` (lowercase, unescaped; the caller escapes each part).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needle {
    pub head: String,
    pub gap: usize,
    pub tail: String,
}

impl Needle {
    fn plain(text: String) -> Self {
        Needle {
            head: text,
            gap: 0,
            tail: String::new(),
        }
    }

    /// Whether `text` holds this needle, as `LIKE '%needle%'` would.
    pub fn found_in(&self, text: &str) -> bool {
        let hay: Vec<char> = text.chars().collect();
        let head: Vec<char> = self.head.chars().collect();
        let tail: Vec<char> = self.tail.chars().collect();
        let len = head.len() + self.gap + tail.len();
        hay.windows(len.max(1))
            .any(|w| w.len() == len && w.starts_with(&head) && w.ends_with(&tail))
    }
}

/// The needles a word may be found by in the loose pass that looks for
/// typos: for a word of [`TYPO_MIN_CHARS`] or more, its first and last
/// letters (two of them below six letters, three from six) and, when those
/// two meet in the middle (four or six letters), the same with its two middle
/// letters free, so one of them survives any single edit or swap of two
/// neighbouring letters; otherwise the word.
pub fn loose_needles(word: &str) -> Vec<Needle> {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    if n < TYPO_MIN_CHARS {
        return vec![Needle::plain(word.to_owned())];
    }
    let k = if n < 6 { 2 } else { 3 };
    let head: String = chars[..k].iter().collect();
    let tail: String = chars[n - k..].iter().collect();
    let mut needles = vec![Needle::plain(head)];
    if !needles.iter().any(|x| x.head == tail) {
        needles.push(Needle::plain(tail));
    }
    if 2 * k == n {
        needles.push(Needle {
            head: chars[..k - 1].iter().collect(),
            gap: 2,
            tail: chars[k + 1..].iter().collect(),
        });
    }
    needles
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn every_word_must_be_held_in_any_order_and_any_name() {
        let album = ["Go: The Very Best of Moby", "Moby"];
        assert!(score(&album, "go moby").is_some());
        assert!(score(&album, "MOBY go").is_some());
        assert!(score(&album, "go moby porcelain").is_none());
        assert!(score(&["Moby"], "go moby").is_none());
    }

    #[test]
    fn one_typo_in_a_long_enough_word_still_matches() {
        assert_eq!(word_match("mboy", &["moby".into()]), Some(WordMatch::Typo));
        assert_eq!(word_match("mopy", &["moby".into()]), Some(WordMatch::Typo));
        assert_eq!(
            word_match("porcelian", &["porcelain".into()]),
            Some(WordMatch::Typo)
        );
        // A typo in a word still being typed.
        assert_eq!(
            word_match("porcl", &["porcelain".into()]),
            Some(WordMatch::Typo)
        );
        // Short words never carry a typo.
        assert_eq!(word_match("og", &["go".into()]), None);
        assert_eq!(word_match("mxyz", &["moby".into()]), None);
    }

    #[test]
    fn closer_matches_rank_first() {
        let exact = score(&["Dragon Ball"], "dragon ball").unwrap();
        let longer = score(&["Dragon Ball GT"], "dragon ball").unwrap();
        let typo = score(&["Dragon Ball"], "dragn ball").unwrap();
        let prefix = score(&["Dragon Ball"], "drag ball").unwrap();
        assert!(exact.is_exact());
        assert!(exact < longer && longer < prefix && prefix < typo);
        assert!(typo.has_typo() && !prefix.has_typo());
        // Named outright beats named with words scattered across names.
        let album = score(&["Go: The Very Best of Moby", "Moby"], "go moby").unwrap();
        let scattered = score(&["Gold", "Moby"], "go moby").unwrap();
        assert!(album < scattered);
    }

    #[test]
    fn any_single_edit_leaves_a_loose_needle_in_place() {
        for (word, typo) in [
            ("moby", "mxby"),
            ("moby", "mob"),
            ("porcelain", "porcxlain"),
            ("porcelain", "porcelaix"),
            ("moby", "mboy"),
            ("moby", "omby"),
            ("moby", "moyb"),
            ("abcdef", "abdcef"),
            ("abcde", "acbde"),
            ("abcde", "abdce"),
        ] {
            let found = loose_needles(typo).iter().any(|n| n.found_in(word));
            assert!(found, "{typo} -> {word}");
        }
        assert_eq!(loose_needles("go"), [Needle::plain("go".into())]);
    }

    #[test]
    fn labels_are_distinct() {
        let all = [
            WordMatch::Exact,
            WordMatch::Prefix,
            WordMatch::Inside,
            WordMatch::Typo,
        ];
        let mut labels: Vec<_> = all.iter().map(|m| m.label()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
    }
}
