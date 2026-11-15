//! Jumping the shelf to a letter, as the web, phone and TV rails do (V1442).
//!
//! The shelf comes in pharos's `SortBy=SortName` order, so the letter a name
//! is filed under follows the same rule as `pharos_core::sort_name`: one
//! leading "the", "a" or "an" does not count, and a name that does not start
//! A to Z is under `#`.

/// The letters a jump can offer, in order: `#`, then A to Z.
pub const LETTERS: [char; 27] = [
    '#', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R',
    'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

const ARTICLES: [&str; 3] = ["the", "a", "an"];

/// The letter `name` is filed under.
#[must_use]
pub fn letter_of(name: &str) -> char {
    let folded = name.trim().to_lowercase();
    let body = ARTICLES
        .iter()
        .find_map(|article| {
            let rest = folded.strip_prefix(article)?;
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            let rest = rest.trim_start();
            (!rest.is_empty()).then_some(rest)
        })
        .unwrap_or(&folded);
    match body.chars().next() {
        Some(first) if first.is_ascii_alphabetic() => first.to_ascii_uppercase(),
        _ => '#',
    }
}

/// Where each letter starts among `names`, in listing order: the letter and
/// the index of the first name under it. A letter no name is under is left
/// out, so a jump is never offered to nowhere.
#[must_use]
pub fn starts<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<(char, usize)> {
    let mut starts: Vec<(char, usize)> = Vec::new();
    for (index, name) in names.into_iter().enumerate() {
        let letter = letter_of(name);
        if !starts.iter().any(|&(seen, _)| seen == letter) {
            starts.push((letter, index));
        }
    }
    starts.sort_by_key(|&(letter, _)| LETTERS.iter().position(|&l| l == letter));
    starts
}

/// The page of `pages` that holds the entry at `index`.
#[must_use]
pub fn page_holding(pages: &[Vec<usize>], index: usize) -> Option<usize> {
    pages.iter().position(|page| page.contains(&index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_article_is_not_the_letter() {
        assert_eq!(letter_of("The Long Road"), 'L');
        assert_eq!(letter_of("a Quiet Year"), 'Q');
        assert_eq!(letter_of("An Hour"), 'H');
        assert_eq!(letter_of("Theory"), 'T', "an article must be a whole word");
        assert_eq!(
            letter_of("The"),
            'T',
            "a name that is only an article keeps it"
        );
    }

    #[test]
    fn a_name_not_starting_a_to_z_is_under_the_hash() {
        assert_eq!(letter_of("1984"), '#');
        assert_eq!(letter_of("  "), '#');
        assert_eq!(letter_of("Élan"), '#');
        assert_eq!(letter_of("zebra"), 'Z');
    }

    #[test]
    fn each_letter_starts_at_its_first_name_and_missing_letters_are_left_out() {
        let names = ["10 Songs", "Alpha", "The Beta", "Bravo", "Delta"];
        assert_eq!(starts(names), vec![('#', 0), ('A', 1), ('B', 2), ('D', 4)]);
    }

    #[test]
    fn the_page_holding_an_entry_is_found() {
        let pages = vec![vec![0, 1, 2], vec![3, 4, 5], vec![6]];
        assert_eq!(page_holding(&pages, 4), Some(1));
        assert_eq!(page_holding(&pages, 6), Some(2));
        assert_eq!(page_holding(&pages, 9), None);
    }
}
