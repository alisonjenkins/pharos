//! The Books screens driven the way the runtime drives them.

use super::*;

const BOOKS: &str = r#"{"Items":[
    {"Id":"b1","Name":"Book One","Path":"/b/one.txt",
     "People":[{"Name":"Author A","Type":"Author"}]},
    {"Id":"b2","Name":"Book Two","Path":"/b/two.pdf"}
],"TotalRecordCount":2}"#;

const PROSE: &str = "Chapter one.\n\nSome prose to read.";

/// Signed in, with the Books list shown and the downloaded-books record empty.
fn books() -> (AppRunner<Pharos>, Vec<Command>) {
    books_from(BOOKS)
}

/// [`books`] with `list` as Pharos's answer for the list.
fn books_from(list_body: &str) -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, task) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let Task::Fetch { headers, .. } = task else {
        panic!("the list is a GET");
    };
    assert!(headers
        .iter()
        .any(|header| header.value.contains("Token=\"t0k\"")));
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: None,
    });
    let commands = runner.task_outcome(list, ok(list_body));
    (runner, commands)
}

/// Taps the book at `index`, which opens its detail page, then its Read
/// button; returns the commands of the Read.
fn read(runner: &mut AppRunner<Pharos>, index: usize) -> Vec<Command> {
    let _ = runner.action(action_id(&format!("book-{index}")));
    runner.action(action_id("book-read"))
}

/// Taps the first book and delivers `body` as its whole file; returns the
/// commands of the delivery.
fn downloaded(runner: &mut AppRunner<Pharos>, body: &str) -> Vec<Command> {
    let commands = read(runner, 0);
    let (download, task) = asked(&commands, "/Items/b1/Download").expect("download");
    let Task::Fetch {
        headers, offset, ..
    } = task
    else {
        panic!("a download is a GET");
    };
    assert_eq!(offset, 0);
    assert!(headers
        .iter()
        .any(|header| header.value.contains("Token=\"t0k\"")));
    runner.task_outcome(download, ok(body))
}

fn wrote(commands: &[Command]) -> Option<(String, Vec<u8>)> {
    commands.iter().find_map(|command| match command {
        Command::Store(StoreRequest::ShelfWrite { name, bytes, .. }) => {
            Some((name.clone(), bytes.clone()))
        }
        _ => None,
    })
}

#[test]
fn the_books_section_opens_the_book_list_and_back_returns_home() {
    let (_, commands) = books();
    let screen = last_screen(&commands).expect("a screen");
    assert!(screen_text(&screen).contains("Book One"));
    let (mut runner, _) = books();
    let commands = runner.action(ActionId::BACK);
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("Audiobooks"), "{screen}");
}

#[test]
fn the_shelves_no_longer_carry_a_books_action() {
    let (_, commands) = shelf();
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(!screen.contains("Books"), "{screen}");
}

#[test]
fn the_list_shows_titles_authors_and_what_cannot_be_opened() {
    let (_, commands) = books();
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("Book One"), "{screen}");
    assert!(screen.contains("Author A"), "{screen}");
    assert!(screen.contains("Can't open here"), "{screen}");
    assert!(!screen.contains("Held"), "{screen}");
}

#[test]
fn a_book_downloads_saves_opens_and_is_marked_downloaded() {
    let (mut runner, _) = books();
    let commands = downloaded(&mut runner, PROSE);
    let (file, bytes) = wrote(&commands).expect("the book goes to the shelf");
    assert_eq!(file, "book-b1.txt");
    assert_eq!(bytes, PROSE.as_bytes());

    let commands = runner.store_result(StoreResult::ShelfWritten {
        name: file,
        size: u32::try_from(PROSE.len()).expect("small"),
    });
    let saved = commands
        .iter()
        .find_map(|command| match command {
            Command::Store(StoreRequest::Save { key, value }) if key == books::KEY => {
                Some(value.clone())
            }
            _ => None,
        })
        .expect("the downloaded list is saved");
    let shelf = books::Shelf::decode(&saved).expect("a record this app reads");
    assert_eq!(shelf.saved.len(), 1);
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        screen.contains("Some prose to read"),
        "the book is open: {screen}"
    );

    let _ = runner.store_result(StoreResult::Saved {
        key: books::KEY.to_owned(),
    });
    let commands = runner.action(ActionId::BACK);
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("Held"), "the downloaded mark: {screen}");
}

#[test]
fn a_pdf_says_this_reader_cannot_open_it_and_downloads_nothing() {
    let (mut runner, _) = books();
    let commands = runner.action(action_id("book-1"));
    assert!(asked(&commands, "/Download").is_none());
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("can't open PDF books yet"), "{screen}");
}

#[test]
fn a_file_that_is_not_the_format_promised_is_refused_before_it_is_saved() {
    let (mut runner, _) = books();
    let commands = downloaded(&mut runner, "");
    assert!(wrote(&commands).is_none());
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("can't open"), "{screen}");
}

#[test]
fn a_kindle_book_pharos_could_not_convert_says_so() {
    let (mut runner, _) = books_from(
        r#"{"Items":[{"Id":"b1","Name":"Book One","Path":"/b/one.azw3"}],"TotalRecordCount":1}"#,
    );
    let commands = downloaded(&mut runner, "BOOKMOBI");
    assert!(wrote(&commands).is_none());
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        screen.contains("couldn't convert this Kindle book"),
        "{screen}"
    );
}

#[test]
fn a_full_piece_asks_for_the_next_from_where_it_ended() {
    let (mut runner, _) = books();
    let commands = read(&mut runner, 0);
    let (download, _) = asked(&commands, "/Items/b1/Download").expect("download");
    let piece = "x".repeat(books::CHUNK as usize);
    let commands = runner.task_outcome(download, ok(&piece));
    let (_, task) = asked(&commands, "/Items/b1/Download").expect("the next piece");
    assert!(matches!(task, Task::Fetch { offset, .. } if offset == books::CHUNK));
}

#[test]
fn a_rejected_token_while_downloading_signs_in_again() {
    let (mut runner, _) = books();
    let commands = read(&mut runner, 0);
    let (download, _) = asked(&commands, "/Items/b1/Download").expect("download");
    let commands = runner.task_outcome(download, TaskOutcome::Failed(TaskError::Unauthorized));
    assert!(asked(&commands, "/QuickConnect/Initiate").is_some());
}

#[test]
fn a_downloaded_book_reopens_from_the_shelf_and_its_menu_removes_it() {
    let record = books::Shelf {
        saved: vec![books::Saved {
            id: "b1".to_owned(),
            file: "book-b1.txt".to_owned(),
            size: PROSE.len(),
            memory: String::new(),
        }],
    }
    .encode();
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, _) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: Some(record),
    });
    let commands = runner.task_outcome(list, ok(BOOKS));
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("Held"), "the downloaded mark: {screen}");

    let commands = read(&mut runner, 0);
    assert!(asked(&commands, "/Download").is_none(), "nothing to fetch");
    assert!(commands.iter().any(|command| matches!(
        command,
        Command::Store(StoreRequest::ShelfRead { name, .. }) if name == "book-b1.txt"
    )));
    let commands = runner.store_result(StoreResult::ShelfRead {
        name: "book-b1.txt".to_owned(),
        offset: 0,
        size: u32::try_from(PROSE.len()).expect("small"),
        bytes: PROSE.as_bytes().to_vec(),
    });
    let reader = last_screen(&commands).expect("a screen");
    let screen = screen_text(&reader);
    assert!(screen.contains("Some prose to read"), "{screen}");
    assert!(reader.owns_back, "Back in a book must stay in Pharos");
    let commands = runner.action(ActionId::BACK);
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        screen.contains("Book One"),
        "Back returns to the list: {screen}"
    );
    let _ = runner.store_result(StoreResult::Saved {
        key: books::KEY.to_owned(),
    });

    let _ = runner.action(action_id("book-menu-0"));
    let commands = runner.action(action_id("book-remove"));
    assert!(commands.iter().any(|command| matches!(
        command,
        Command::Store(StoreRequest::ShelfRemove { name }) if name == "book-b1.txt"
    )));
    let commands = runner.store_result(StoreResult::ShelfRemoved {
        name: "book-b1.txt".to_owned(),
    });
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(!screen.contains("· Downloaded"), "{screen}");
    assert!(commands.iter().any(|command| matches!(
        command,
        Command::Store(StoreRequest::Save { key, .. }) if key == books::KEY
    )));
}

#[test]
fn a_damaged_saved_copy_is_downloaded_again_on_the_next_open() {
    let record = books::Shelf {
        saved: vec![books::Saved {
            id: "b1".to_owned(),
            file: "book-b1.txt".to_owned(),
            size: 99,
            memory: String::new(),
        }],
    }
    .encode();
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, _) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: Some(record),
    });
    let _ = runner.task_outcome(list, ok(BOOKS));
    let _ = read(&mut runner, 0);
    let _ = runner.store_result(StoreResult::ShelfRead {
        name: "book-b1.txt".to_owned(),
        offset: 0,
        size: 3,
        bytes: b"Bad".to_vec(),
    });
    let commands = read(&mut runner, 0);
    assert!(asked(&commands, "/Items/b1/Download").is_some());
}

#[test]
fn the_books_screens_fit_the_smallest_reader() {
    let (mut runner, commands) = books();
    let mut screens: Vec<Screen> = last_screen(&commands).into_iter().collect();
    let commands = runner.action(action_id("book-1"));
    screens.extend(last_screen(&commands));
    let commands = read(&mut runner, 0);
    screens.extend(last_screen(&commands));
    let (download, _) = asked(&commands, "/Items/b1/Download").expect("download");
    let commands = runner.task_outcome(download, ok(PROSE));
    screens.extend(last_screen(&commands));
    let (file, _) = wrote(&commands).expect("shelf write");
    let commands = runner.store_result(StoreResult::ShelfWritten {
        name: file,
        size: u32::try_from(PROSE.len()).expect("small"),
    });
    screens.extend(last_screen(&commands));
    let _ = runner.store_result(StoreResult::Saved {
        key: books::KEY.to_owned(),
    });
    let _ = runner.action(ActionId::BACK);
    let _ = runner.action(action_id("book-menu-0"));
    screens.extend(last_screen(&runner.action(ActionId::BACK)));
    for (index, screen) in screens.iter().enumerate() {
        let problems = screen.validate(&CLARA_BW_METRICS);
        assert!(problems.is_empty(), "screen {index}: {problems:?}");
    }
}

#[test]
fn the_books_list_is_a_grid_of_covers_drawn_together_and_dropped_on_leaving() {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, _) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: None,
    });
    let listing = r#"{"Items":[
        {"Id":"b1","Name":"Book One","Path":"/b/one.txt","ImageTags":{"Primary":"c1"}},
        {"Id":"b2","Name":"Book Two","Path":"/b/two.txt","ImageTags":{"Primary":"c2"}},
        {"Id":"b3","Name":"Book Three","Path":"/b/three.txt"}
    ],"TotalRecordCount":3}"#;
    let commands = runner.task_outcome(list, ok(listing));
    let screen = screen_text(&last_screen(&commands).expect("the grid"));
    assert!(screen.contains("TileGrid"), "covers, not rows: {screen}");
    assert!(screen.contains("Portrait"), "{screen}");
    let covers = cover_tasks(&commands);
    assert_eq!(covers.len(), 2, "a cover for each tagged book: {covers:?}");
    assert!(
        covers[0].1.contains("/Items/b1/Images/Primary"),
        "{covers:?}"
    );
    let drawn = page_drawn(&mut runner, &commands);
    let text = screen_text(&drawn);
    assert_eq!(text.matches("picture: Some").count(), 2, "{text}");
    let problems = drawn.validate(&CLARA_BW_METRICS);
    assert!(
        problems.is_empty(),
        "fits the smallest reader: {problems:?}"
    );
    // Leaving the books drops the covers still wanted for them.
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, _) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let commands = runner.task_outcome(list, ok(listing));
    let covers = cover_tasks(&commands);
    let commands = runner.action(ActionId::BACK);
    for (task, _) in &covers {
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, Command::Cancel(id) if id == task)),
            "{commands:?}"
        );
    }
}

#[test]
fn the_books_list_loads_whole_and_jumps_to_a_letter() {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, _) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: None,
    });
    let block = |from: usize, to: usize, total: usize| {
        let items: Vec<String> = (from..to)
            .map(|index| {
                let title = if index < 40 {
                    format!("Apple {index}")
                } else {
                    format!("The Zest {index}")
                };
                format!(r#"{{"Id":"b{index}","Name":"{title}","Path":"/b/{index}.txt"}}"#)
            })
            .collect();
        format!(
            r#"{{"Items":[{}],"TotalRecordCount":{total}}}"#,
            items.join(",")
        )
    };
    let commands = runner.task_outcome(list, ok(&block(0, 30, 60)));
    let (rest, _) = asked(&commands, "StartIndex=30").expect("the rest is asked for at once");
    let _ = runner.task_outcome(rest, ok(&block(30, 60, 60)));
    let commands = runner.action(action_id("letters"));
    let picker = screen_text(&last_screen(&commands).expect("the picker"));
    assert!(picker.contains("Jump to a letter"), "{picker}");
    let commands = runner.action(action_id("letter-Z"));
    let list = screen_text(&last_screen(&commands).expect("the list"));
    assert!(!list.contains("Jump to a letter"), "the picker closes");
    assert!(
        list.contains("The Zest 40"),
        "the first Z book shows: {list}"
    );
    assert!(!list.contains("Apple 0\""), "{list}");
}

#[test]
fn a_tap_opens_the_book_s_detail_page_before_anything_downloads() {
    let (mut runner, _) = books();
    let commands = runner.action(action_id("book-0"));
    assert!(
        asked(&commands, "/Download").is_none(),
        "a tap only opens it"
    );
    let detail = last_screen(&commands).expect("a screen");
    let text = screen_text(&detail);
    for part in ["Book One", "Author A", "Download and read"] {
        assert!(text.contains(part), "{part} missing from {text}");
    }
    assert!(!text.contains("Remove download"), "not downloaded: {text}");
    assert!(detail.validate(&CLARA_BW_METRICS).is_empty());
    let commands = runner.action(ActionId::BACK);
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        text.contains("TileGrid"),
        "Back returns to the grid: {text}"
    );
}

#[test]
fn a_book_this_reader_cannot_open_says_why_on_its_detail_page() {
    let (mut runner, _) = books();
    let text = screen_text(&last_screen(&runner.action(action_id("book-1"))).expect("a screen"));
    assert!(text.contains("can't open PDF books yet"), "{text}");
    assert!(!text.contains("book-read"), "nothing to read: {text}");
}

#[test]
fn a_downloaded_book_s_detail_page_reads_and_removes() {
    let record = books::Shelf {
        saved: vec![books::Saved {
            id: "b1".to_owned(),
            file: "book-b1.txt".to_owned(),
            size: PROSE.len(),
            memory: String::new(),
        }],
    }
    .encode();
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, _) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: Some(record),
    });
    // The longest teaser, the downloaded fact and every button at once.
    let listing = format!(
        r#"{{"Items":[{{"Id":"b1","Name":"Book One","Path":"/b/one.txt",
            "People":[{{"Name":"Author A","Type":"Author"}}],"Overview":"{}"}}],
            "TotalRecordCount":1}}"#,
        "A made-up description that runs on. ".repeat(20)
    );
    let _ = runner.task_outcome(list, ok(&listing));
    for open in ["book-0", "book-menu-0"] {
        let detail = last_screen(&runner.action(action_id(open))).expect("a screen");
        let text = screen_text(&detail);
        for part in [
            "Downloaded",
            "\"Read\"",
            "Remove download",
            "About this book",
        ] {
            assert!(text.contains(part), "{open}: {part} missing from {text}");
        }
        assert!(detail.validate(&CLARA_BW_METRICS).is_empty());
        let _ = runner.action(ActionId::BACK);
    }
}

#[test]
fn an_open_detail_page_keeps_its_cover_wanted() {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-books"));
    let (list, _) = asked(&commands, "IncludeItemTypes=Book").expect("book list");
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: None,
    });
    let listing = r#"{"Items":[
        {"Id":"b1","Name":"Book One","Path":"/b/one.txt","ImageTags":{"Primary":"c1"}}
    ],"TotalRecordCount":1}"#;
    let commands = runner.task_outcome(list, ok(listing));
    let covers = cover_tasks(&commands);
    assert_eq!(covers.len(), 1, "{covers:?}");
    let commands = runner.action(action_id("book-0"));
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, Command::Cancel(id) if *id == covers[0].0)),
        "the detail page's cover is still on its way: {commands:?}"
    );
    let commands = runner.task_outcome(covers[0].0, cover_bytes());
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        text.contains("Picture"),
        "the cover beside the title: {text}"
    );
}

/// Prose long enough to take several pages.
fn long_opening() -> String {
    (0..60)
        .map(|n| format!("Paragraph {n} of a made-up opening that goes on for a while."))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Opens the first book's detail page and taps Read the opening; returns the
/// task asking for it.
fn ask_opening(runner: &mut AppRunner<Pharos>) -> TaskId {
    let _ = runner.action(action_id("book-0"));
    let commands = runner.action(action_id("book-opening"));
    let (task, request) = asked(&commands, "/Items/b1/Opening").expect("the opening is asked for");
    let Task::Fetch {
        headers,
        offset,
        max_bytes,
        ..
    } = request
    else {
        panic!("the opening is a GET");
    };
    assert_eq!(offset, 0);
    assert!(max_bytes >= 32 * 1024);
    assert!(headers
        .iter()
        .any(|header| header.value.contains("Token=\"t0k\"")));
    assert!(screen_text(&last_screen(&commands).expect("a screen")).contains("Loading the opening"));
    task
}

#[test]
fn a_readable_book_offers_its_opening_and_an_unreadable_one_does_not() {
    let (mut runner, _) = books();
    let detail = last_screen(&runner.action(action_id("book-0"))).expect("a screen");
    assert!(screen_text(&detail).contains("Read the opening"));
    let _ = runner.action(ActionId::BACK);
    let detail = last_screen(&runner.action(action_id("book-1"))).expect("a screen");
    assert!(!screen_text(&detail).contains("Read the opening"));
}

#[test]
fn the_opening_is_shown_on_pages_and_back_returns_to_the_detail_page() {
    let (mut runner, _) = books();
    let task = ask_opening(&mut runner);
    let first = last_screen(&runner.task_outcome(task, ok(&long_opening()))).expect("a screen");
    let text = screen_text(&first);
    assert!(text.contains("Opening"), "{text}");
    assert!(text.contains("Paragraph 0 of"), "{text}");
    assert!(first.validate(&CLARA_BW_METRICS).is_empty());

    let next = last_screen(&runner.action(action_id("book-opening-next"))).expect("a screen");
    assert_ne!(screen_text(&next), text, "the page turns");
    let back = last_screen(&runner.action(action_id("book-opening-back"))).expect("a screen");
    assert_eq!(screen_text(&back), text);

    let detail = screen_text(&last_screen(&runner.action(ActionId::BACK)).expect("a screen"));
    assert!(detail.contains("Read the opening"), "{detail}");
    assert!(detail.contains("Book One"), "{detail}");
}

#[test]
fn leaving_the_detail_page_drops_the_opening() {
    let (mut runner, _) = books();
    let task = ask_opening(&mut runner);
    let _ = runner.task_outcome(task, ok(PROSE));
    let _ = runner.action(ActionId::BACK);
    let _ = runner.action(ActionId::BACK);
    let _ = runner.action(action_id("book-0"));
    let commands = runner.action(action_id("book-opening"));
    assert!(
        asked(&commands, "/Items/b1/Opening").is_some(),
        "a new visit asks again: {commands:?}"
    );
}

#[test]
fn a_book_without_an_opening_says_so_on_its_detail_page() {
    let (mut runner, _) = books();
    let task = ask_opening(&mut runner);
    let commands = runner.task_outcome(task, TaskOutcome::Failed(TaskError::NotFound));
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        text.contains("This book has no opening Pharos can show."),
        "{text}"
    );
    assert!(
        text.contains("Read the opening"),
        "still the detail page: {text}"
    );
}

#[test]
fn another_failure_shows_the_usual_words() {
    let (mut runner, _) = books();
    let task = ask_opening(&mut runner);
    let commands = runner.task_outcome(task, TaskOutcome::Failed(TaskError::Offline));
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("The reader is offline"), "{text}");
    assert!(text.contains("Read the opening"), "{text}");
}

#[test]
fn backing_out_while_the_opening_loads_cancels_it() {
    let (mut runner, _) = books();
    let task = ask_opening(&mut runner);
    let commands = runner.action(ActionId::BACK);
    assert!(commands
        .iter()
        .any(|command| matches!(command, Command::Cancel(id) if *id == task)));
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("Read the opening"), "{text}");
}
