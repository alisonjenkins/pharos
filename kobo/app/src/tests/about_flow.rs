//! Descriptions on the audiobook and ebook detail screens.

use super::*;
use kobo_json::{ObjectBuilder, Value};

/// A description far longer than one detail screen, in paragraphs.
fn long_description() -> String {
    (0..40)
        .map(|index| {
            format!(
                "Paragraph {index} of a made-up description, long enough to wrap \
                 over several lines of the smallest reader."
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The audiobooks shelf holding one audiobook described by `overview`.
fn described_shelf(overview: &str) -> (AppRunner<Pharos>, Vec<Command>) {
    let mut runner = AppRunner::new(Pharos::default());
    let _ = runner.start();
    let commands = runner.store_result(StoreResult::Loaded {
        key: ACCOUNT_KEY.to_owned(),
        value: Some(account().encode()),
    });
    let (views, _) = asked(&commands, "/Users/u1/Views").expect("views");
    let _ = runner.task_outcome(views, ok(VIEWS));
    let commands = runner.action(action_id("section-audiobooks"));
    let (shelf, _) = asked(&commands, "ParentId=lib-books").expect("audiobooks");
    let body = ObjectBuilder::new()
        .set(
            "Items",
            Value::Array(vec![ObjectBuilder::new()
                .set("Id", "bk1")
                .set("Name", "Book One")
                .set("AlbumArtist", "Author A")
                .set("PharosAudiobook", true)
                .set("Overview", overview)
                .build()]),
        )
        .set("TotalRecordCount", 1)
        .build()
        .to_json();
    let commands = runner.task_outcome(shelf, ok(&body));
    (runner, commands)
}

#[test]
fn a_short_audiobook_description_sits_whole_under_the_masthead() {
    let (mut runner, _) = described_shelf("A short made-up tale.");
    let screen = last_screen(&runner.action(action_id("tile-0"))).expect("a screen");
    let text = screen_text(&screen);
    assert!(text.contains("A short made-up tale."), "{text}");
    assert!(!text.contains("About"), "nothing more to show: {text}");
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());
}

#[test]
fn a_long_audiobook_description_opens_on_pages_of_its_own() {
    let description = long_description();
    let (mut runner, _) = described_shelf(&description);
    let album = last_screen(&runner.action(action_id("tile-0"))).expect("a screen");
    let text = screen_text(&album);
    assert!(text.contains("Paragraph 0 of a made-up"), "{text}");
    assert!(!text.contains("Paragraph 5 "), "only a teaser: {text}");
    assert!(text.contains("About"), "{text}");
    assert!(album.validate(&CLARA_BW_METRICS).is_empty());

    let first = last_screen(&runner.action(action_id("about"))).expect("a screen");
    assert!(screen_text(&first).contains("Paragraph 0 of"));
    let mut seen = vec![screen_text(&first)];
    for _ in 0..40 {
        let Some(screen) = last_screen(&runner.action(action_id("page-next"))) else {
            break;
        };
        let problems = screen.validate(&CLARA_BW_METRICS);
        assert!(problems.is_empty(), "{problems:?}");
        if seen.last() == Some(&screen_text(&screen)) {
            break;
        }
        seen.push(screen_text(&screen));
    }
    assert!(seen.len() > 1, "a long description takes several pages");
    assert!(
        seen.last()
            .is_some_and(|last| last.contains("Paragraph 39 of")),
        "the last paragraph is reached"
    );
    let back = last_screen(&runner.action(ActionId::BACK)).expect("a screen");
    assert!(
        screen_text(&back).contains("About"),
        "Back returns to the album"
    );
}

#[test]
fn an_audiobook_with_no_description_shows_none() {
    let (mut runner, _) = described_shelf("");
    let text = screen_text(&last_screen(&runner.action(action_id("tile-0"))).expect("a screen"));
    assert!(!text.contains("About"), "{text}");
}

/// The Books list holding one ebook described by `overview`.
fn described_books(overview: &str) -> AppRunner<Pharos> {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, task) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    assert!(url_of(&task).is_some_and(|url| url.contains("Overview")));
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: None,
    });
    let body = ObjectBuilder::new()
        .set(
            "Items",
            Value::Array(vec![ObjectBuilder::new()
                .set("Id", "b1")
                .set("Name", "Book One")
                .set("Path", "/b/one.txt")
                .set("Overview", overview)
                .build()]),
        )
        .set("TotalRecordCount", 1)
        .build()
        .to_json();
    let _ = runner.task_outcome(list, ok(&body));
    runner
}

#[test]
fn an_ebook_menu_shows_its_description_and_opens_the_rest() {
    let description = long_description();
    let mut runner = described_books(&description);
    let menu = last_screen(&runner.action(action_id("book-menu-0"))).expect("a screen");
    let text = screen_text(&menu);
    assert!(text.contains("Paragraph 0 of a made-up"), "{text}");
    assert!(text.contains("About this book"), "{text}");
    assert!(menu.validate(&CLARA_BW_METRICS).is_empty());

    let first = last_screen(&runner.action(action_id("book-about"))).expect("a screen");
    assert!(screen_text(&first).contains("Paragraph 0 of"));
    assert!(first.validate(&CLARA_BW_METRICS).is_empty());
    let next = last_screen(&runner.action(action_id("book-about-next"))).expect("a screen");
    assert_ne!(screen_text(&next), screen_text(&first), "the page turns");
    let back = last_screen(&runner.action(action_id("book-about-back"))).expect("a screen");
    assert_eq!(screen_text(&back), screen_text(&first));

    let menu_again = last_screen(&runner.action(ActionId::BACK)).expect("a screen");
    assert!(
        screen_text(&menu_again).contains("About this book"),
        "Back returns to the menu"
    );
}

#[test]
fn a_short_ebook_description_needs_no_about_button() {
    let mut runner = described_books("A short made-up tale.");
    let text =
        screen_text(&last_screen(&runner.action(action_id("book-menu-0"))).expect("a screen"));
    assert!(text.contains("A short made-up tale."), "{text}");
    assert!(!text.contains("About this book"), "{text}");
}
