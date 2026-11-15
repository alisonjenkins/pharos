//! A book's or an audiobook's description: a few lines on its detail screen,
//! and the whole text on pages of its own when those lines cut it short.

use kobo_sdk::prelude::*;

/// The most characters of a description shown on a detail screen. About
/// three lines of the smallest reader's text, so the rows beneath keep the
/// panel; the rest is one tap away on the description's own pages.
pub const TEASER_CHARS: usize = 180;

/// The description as it appears on a detail screen: its first
/// `max_chars` characters on one line of prose, cut at a word with an
/// ellipsis, and whether anything was left out. `None` when there is no
/// description.
#[must_use]
pub fn teaser(text: &str, max_chars: usize) -> Option<(String, bool)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let mut shown = String::new();
    for word in &words {
        let wanted = shown.chars().count() + usize::from(!shown.is_empty()) + word.chars().count();
        if wanted > max_chars {
            if shown.is_empty() {
                shown = word.chars().take(max_chars).collect();
            }
            shown.push('…');
            return Some((shown, true));
        }
        if !shown.is_empty() {
            shown.push(' ');
        }
        shown.push_str(word);
    }
    Some((shown, false))
}

/// The paragraphs of a description, blank lines between them, each joined
/// onto one line: pharos sends plain text whose line breaks are wherever the
/// source wrapped it.
#[must_use]
pub fn paragraphs(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The description broken into pages that fit this panel under a top bar,
/// with page turns at the foot.
#[must_use]
pub fn pages(context: &Context, text: &str) -> Vec<Vec<String>> {
    context.paginate(&paragraphs(text), true)
}

/// One page of the description, `page` counted from zero and held to the
/// last. `back` and `next` name the page-turn actions.
#[must_use]
pub fn screen(
    context: &Context,
    id: &str,
    title: &str,
    text: &str,
    page: usize,
    (back, next): (&str, &str),
) -> ScreenBuilder {
    let pages = pages(context, text);
    let page = page.min(pages.len().saturating_sub(1));
    let mut screen = ScreenBuilder::new(id).top_bar(title.to_owned());
    for paragraph in pages.get(page).into_iter().flatten() {
        screen = screen.text(paragraph.clone());
    }
    screen.page_turns(back, next)
}

/// How many pages the description takes, never fewer than one.
#[must_use]
pub fn page_count(context: &Context, text: &str) -> usize {
    pages(context, text).len().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_description_is_shown_whole() {
        assert_eq!(
            teaser("A  short\ntale.", 40),
            Some(("A short tale.".to_owned(), false))
        );
    }

    #[test]
    fn a_long_description_is_cut_at_a_word() {
        let (shown, cut) = teaser("one two three four", 12).unwrap_or_default();
        assert_eq!(shown, "one two…");
        assert!(cut);
    }

    #[test]
    fn a_word_longer_than_the_room_is_cut_inside_it() {
        let (shown, cut) = teaser("abcdefghij more", 4).unwrap_or_default();
        assert_eq!(shown, "abcd…");
        assert!(cut);
    }

    #[test]
    fn no_description_shows_nothing() {
        assert_eq!(teaser(" \n ", 40), None);
    }

    #[test]
    fn paragraphs_keep_their_breaks_and_lose_the_source_wrapping() {
        assert_eq!(paragraphs("one\ntwo\n\n\n three "), "one two\n\nthree");
    }
}
