//! The Books filters driven the way the runtime drives them (V1449, V1451).

use super::*;

const BOOKS: &str = r#"{"Items":[
    {"Id":"b1","Name":"Book One","Path":"/b/one.epub"},
    {"Id":"b2","Name":"Book Two","Path":"/b/two.epub"}
],"TotalRecordCount":2}"#;

const SUBJECTS: &str = r#"{"Items":[{"Name":"History"},{"Name":"Art"}]}"#;

/// Signed in, with the Books list shown and nothing downloaded; also the
/// list's URL.
fn books() -> (AppRunner<Pharos>, String) {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, task) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let url = url_of(&task).expect("a fetch").to_owned();
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: None,
    });
    let _ = runner.task_outcome(list, ok(BOOKS));
    (runner, url)
}

/// Opens the filter screen and answers its subjects; returns its commands.
fn filter(runner: &mut AppRunner<Pharos>) -> Vec<Command> {
    let commands = runner.action(action_id("books-filter"));
    let (subjects, _) = asked(&commands, "/Genres?IncludeItemTypes=Book").expect("subjects");
    let mut all = commands;
    all.extend(runner.task_outcome(subjects, ok(SUBJECTS)));
    all
}

#[test]
fn the_first_list_leaves_out_books_no_client_can_open() {
    let (_, url) = books();
    assert!(url.contains("&IsReadable=true"), "{url}");
}

#[test]
fn the_list_offers_the_filters_and_the_screen_shows_each_group() {
    let (mut runner, _) = books();
    let commands = filter(&mut runner);
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    for words in [
        "Sort by",
        "Published",
        "Read",
        "Unread",
        "Reading",
        "Subject",
        "Downloaded only",
        "Show books",
    ] {
        assert!(screen.contains(words), "{words}: {screen}");
    }
}

#[test]
fn show_books_lists_again_with_the_chosen_read_state_order_and_subject() {
    let (mut runner, _) = books();
    let _ = filter(&mut runner);
    let _ = runner.action(action_id("books-read-unread"));
    let _ = runner.action(action_id("books-order-added"));
    let _ = runner.action(action_id("books-subjects"));
    // Sorted: All subjects, Art, History.
    let commands = runner.action(action_id("books-subject-2"));
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("History"), "{screen}");
    let commands = runner.action(action_id("books-filter-apply"));
    let (list, task) = asked(&commands, "IncludeItemTypes=Book").expect("a new list");
    let url = url_of(&task).expect("a fetch");
    assert!(url.contains("&IsReadable=true"), "{url}");
    assert!(url.contains("&IsPlayed=false"), "{url}");
    assert!(
        url.contains("SortBy=DateCreated,SortName&SortOrder=Descending"),
        "{url}"
    );
    assert!(url.contains("&Genres=History"), "{url}");
    assert!(url.contains("StartIndex=0&"), "{url}");
    let commands = runner.task_outcome(
        list,
        ok(r#"{"Items":[{"Id":"b2","Name":"Book Two","Path":"/b/two.epub"}],"TotalRecordCount":1}"#),
    );
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        screen.contains("Unread · History · by added · 1"),
        "{screen}"
    );
    assert!(!screen.contains("Book One"), "{screen}");
    assert!(
        !screen.contains("A–Z"),
        "letters only in name order: {screen}"
    );
}

#[test]
fn back_from_the_filter_screen_keeps_the_list_as_it_was() {
    let (mut runner, _) = books();
    let _ = filter(&mut runner);
    let _ = runner.action(action_id("books-read-read"));
    let commands = runner.action(ActionId::BACK);
    assert!(asked(&commands, "IncludeItemTypes=Book").is_none());
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("Books · 2"), "{screen}");
    // And the abandoned choice is not what the screen opens on next time.
    let commands = runner.action(action_id("books-filter"));
    let screen = last_screen(&commands).expect("a screen");
    assert!(
        !screen_text(&screen).contains("Read · "),
        "{}",
        screen_text(&screen)
    );
}

#[test]
fn downloaded_only_with_nothing_downloaded_asks_nothing_and_offers_a_way_back() {
    let (mut runner, _) = books();
    let _ = filter(&mut runner);
    let _ = runner.action(action_id("books-downloaded"));
    let commands = runner.action(action_id("books-filter-apply"));
    assert!(asked(&commands, "IncludeItemTypes=Book").is_none());
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("No books match these filters."), "{screen}");
    let commands = runner.action(action_id("books-filter-reset"));
    let (_, task) = asked(&commands, "IncludeItemTypes=Book").expect("the whole list again");
    assert!(!url_of(&task).expect("a fetch").contains("Ids="));
}

#[test]
fn subjects_that_fail_to_load_say_so_and_can_be_asked_again() {
    let (mut runner, _) = books();
    let commands = runner.action(action_id("books-filter"));
    let (subjects, _) = asked(&commands, "/Genres").expect("subjects");
    let _ = runner.task_outcome(subjects, TaskOutcome::Failed(TaskError::NotFound));
    let commands = runner.action(action_id("books-subjects"));
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("could not be loaded"), "{screen}");
    let commands = runner.action(action_id("books-subjects-retry"));
    assert!(asked(&commands, "/Genres").is_some());
}

#[test]
fn subjects_that_arrive_while_their_screen_is_open_are_shown() {
    let (mut runner, _) = books();
    let commands = runner.action(action_id("books-filter"));
    let (subjects, _) = asked(&commands, "/Genres").expect("subjects");
    let commands = runner.action(action_id("books-subjects"));
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("Loading the subjects"), "{screen}");
    let commands = runner.task_outcome(subjects, ok(SUBJECTS));
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        screen.contains("All subjects") && screen.contains("Art"),
        "{screen}"
    );
}

#[test]
fn the_filter_and_subject_screens_fit_the_smallest_reader() {
    let (mut runner, _) = books();
    let mut screens = Vec::new();
    screens.extend(last_screen(&filter(&mut runner)));
    screens.extend(last_screen(&runner.action(action_id("books-subjects"))));
    assert_eq!(screens.len(), 2);
    for (index, screen) in screens.iter().enumerate() {
        let problems = screen.validate(&CLARA_BW_METRICS);
        assert!(problems.is_empty(), "screen {index}: {problems:?}");
    }
}
