//! The home screen's recent rows (V1445), driven the way the runtime drives
//! them.

use super::*;

/// Two tracks of one album and one of another, newest first.
const RESUMED: &str = r#"{"Items":[
    {"Id":"tr2","Name":"Closing","AlbumId":"al1","Album":"First Album",
     "AlbumArtist":"Band","UserData":{"PlaybackPositionTicks":6000000000}},
    {"Id":"tr1","Name":"Opening","AlbumId":"al1","Album":"First Album",
     "UserData":{"PlaybackPositionTicks":10000000}},
    {"Id":"tx","Name":"Other","AlbumId":"al9","Album":"Spoken",
     "UserData":{"PlaybackPositionTicks":20000000}}
]}"#;

const BOOKS: &str = r#"{"Items":[
    {"Id":"b1","Name":"Book One","Path":"/b/one.txt",
     "People":[{"Name":"Author A","Type":"Author"}]}
],"TotalRecordCount":1}"#;

const PROSE: &str = "Chapter one.\n\nSome prose to read.";

/// A signed-in runner whose libraries have loaded, with `record` as the
/// downloaded books, before pharos has said what was played.
fn signed_in(record: Option<Vec<u8>>) -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, commands) = started(Some(&account()));
    let (views, _) = asked(&commands, "/Users/u1/Views").expect("views");
    let mut all = commands;
    all.extend(runner.task_outcome(views, ok(VIEWS)));
    all.extend(runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: record,
    }));
    (runner, all)
}

/// The home screen with `RESUMED` answered.
fn listening() -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, commands) = signed_in(None);
    let (resume, _) = asked(&commands, "/Items/Resume?MediaTypes=Audio").expect("resume");
    let commands = runner.task_outcome(resume, ok(RESUMED));
    (runner, commands)
}

fn record(memory: &str) -> Vec<u8> {
    books::Shelf {
        saved: vec![
            books::Saved {
                id: "b1".to_owned(),
                file: "book-b1.txt".to_owned(),
                size: PROSE.len(),
                memory: memory.to_owned(),
            },
            books::Saved {
                id: "b2".to_owned(),
                file: "book-b2.txt".to_owned(),
                size: 10,
                memory: String::new(),
            },
        ],
    }
    .encode()
}

#[test]
fn the_home_screen_asks_pharos_what_was_played_last() {
    let (_, commands) = signed_in(None);
    let (_, task) = asked(&commands, "/Users/u1/Items/Resume?MediaTypes=Audio").expect("resume");
    let Task::Fetch { headers, .. } = task else {
        panic!("resume is a GET");
    };
    assert!(headers
        .iter()
        .any(|header| header.value.contains("Token=\"t0k\"")));
}

#[test]
fn part_played_albums_lead_the_home_screen_one_tile_each() {
    let (_, commands) = listening();
    let screen = last_screen(&commands).expect("a screen");
    let text = screen_text(&screen);
    assert!(text.contains("Continue listening"), "{text}");
    assert_eq!(text.matches("First Album").count(), 1, "one tile per album");
    assert!(text.contains("Spoken"), "{text}");
    let listening = text.find("Continue listening").unwrap_or(usize::MAX);
    let libraries = text.find("Libraries").unwrap_or(0);
    assert!(listening < libraries, "the recent row comes first: {text}");
    for name in ["Music", "Audiobooks", "Books"] {
        assert!(text.contains(name), "{name} missing from {text}");
    }
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());
    // The newest album is described first, for its cover and its kind.
    assert!(asked(&commands, "/Users/u1/Items/al1").is_some());
}

#[test]
fn a_described_audiobook_gets_its_glyph_and_cover() {
    let (mut runner, commands) = listening();
    let (describe, _) = asked(&commands, "/Users/u1/Items/al1").expect("describe");
    let commands = runner.task_outcome(
        describe,
        ok(r#"{"Id":"al1","Name":"First Album","PharosAudiobook":true,"ImageTags":{"Primary":"c1"}}"#),
    );
    assert!(
        cover_tasks(&commands)
            .iter()
            .any(|(_, url)| url.contains("/Items/al1/Images")),
        "{:?}",
        cover_tasks(&commands)
    );
    // The next album is described in turn, one at a time.
    assert!(asked(&commands, "/Users/u1/Items/al9").is_some());
    let (cover, _) = cover_tasks(&commands)[0].clone();
    let commands = runner.task_outcome(cover, cover_bytes());
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("Headphones"), "{text}");
    assert!(text.contains("Picture"), "{text}");
}

/// Asserts `commands` started `tr2` ten minutes in.
fn started_ten_minutes_in(commands: &[Command]) {
    let (_, task) = asked(commands, "/Sessions/Playing").expect("started report");
    let Task::Post { body, .. } = task else {
        panic!("a report is a POST");
    };
    assert!(body.contains("\"ItemId\":\"tr2\""), "{body}");
    let played = format!("{commands:?}");
    assert!(played.contains("600"), "starts ten minutes in: {played}");
}

#[test]
fn a_tap_opens_the_album_with_a_resume_button_and_plays_nothing_yet() {
    let (mut runner, _) = listening();
    let commands = runner.action(action_id("listen-0"));
    let (tracks, _) = asked(&commands, "ParentId=al1").expect("tracks");
    let commands = runner.task_outcome(tracks, ok(TRACKS));
    assert!(
        asked(&commands, "/Sessions/Playing").is_none(),
        "a tap only opens"
    );
    let screen = last_screen(&commands).expect("a screen");
    let text = screen_text(&screen);
    assert!(text.contains("Resume track 2 at 10:00"), "{text}");
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());

    let commands = runner.action(action_id("resume"));
    started_ten_minutes_in(&commands);

    // Back from the player is the album; Back from the album is home.
    let _ = runner.action(ActionId::BACK);
    let commands = runner.action(ActionId::BACK);
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("Continue listening"), "{text}");
    assert!(
        asked(&commands, "/Items/Resume").is_some(),
        "home asks again, so what played here comes first"
    );
}

#[test]
fn a_hold_carries_on_from_the_track_and_place_it_was_left() {
    let (mut runner, commands) = listening();
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("menu: Some"), "the tile has a hold: {text}");
    let commands = runner.action(action_id("listen-hold-0"));
    let (tracks, _) = asked(&commands, "ParentId=al1").expect("tracks");
    let commands = runner.task_outcome(tracks, ok(TRACKS));
    started_ten_minutes_in(&commands);
}

#[test]
fn resume_pressed_before_the_tracks_arrive_plays_when_they_do() {
    let (mut runner, _) = listening();
    let commands = runner.action(action_id("listen-0"));
    let (tracks, _) = asked(&commands, "ParentId=al1").expect("tracks");
    let commands = runner.action(action_id("resume"));
    assert!(asked(&commands, "/Sessions/Playing").is_none());
    let commands = runner.task_outcome(tracks, ok(TRACKS));
    started_ten_minutes_in(&commands);
}

#[test]
fn an_album_opened_from_its_shelf_does_not_carry_on() {
    let (mut runner, _) = listening();
    let _ = runner.action(action_id("listen-0"));
    let _ = runner.action(ActionId::BACK);
    let commands = runner.action(action_id("section-music"));
    let (albums, _) = asked(&commands, "ParentId=lib-music").expect("albums");
    let _ = runner.task_outcome(albums, ok(ALBUMS));
    let commands = runner.action(action_id("tile-0"));
    let (tracks, _) = asked(&commands, "ParentId=al1").expect("tracks");
    let commands = runner.task_outcome(tracks, ok(TRACKS));
    assert!(
        asked(&commands, "/Sessions/Playing").is_none(),
        "nothing plays"
    );
    let commands = runner.action(ActionId::BACK);
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        !text.contains("Continue listening"),
        "back to the shelf: {text}"
    );
}

#[test]
fn a_failed_resume_list_leaves_the_libraries_alone() {
    let (mut runner, commands) = signed_in(None);
    let (resume, _) = asked(&commands, "/Items/Resume").expect("resume");
    let commands = runner.task_outcome(resume, TaskOutcome::Failed(TaskError::Offline));
    // Nothing on the panel changes, so nothing is drawn again.
    assert!(last_screen(&commands).is_none(), "{commands:?}");
    let commands = runner.action(action_id("section-music"));
    assert!(
        asked(&commands, "ParentId=lib-music").is_some(),
        "home still works"
    );
}

/// The home screen with `b1` read on this reader and described.
fn reading() -> AppRunner<Pharos> {
    let (mut runner, commands) = signed_in(Some(record("at 2\n")));
    let (known, _) = asked(&commands, "Ids=b1&").expect("only the book read is asked about");
    let commands = runner.task_outcome(known, ok(BOOKS));
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("Continue reading"), "{text}");
    assert!(text.contains("Book One"), "{text}");
    assert!(text.contains("Author A"), "{text}");
    runner
}

/// Delivers `b1` from the shelf, asserts it opened, and closes it.
fn read_and_close(runner: &mut AppRunner<Pharos>, commands: &[Command]) -> Vec<Command> {
    assert!(asked(commands, "/Download").is_none(), "nothing to fetch");
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
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("Some prose to read"), "{text}");
    runner.action(ActionId::BACK)
}

#[test]
fn a_tap_on_a_book_opens_its_detail_page_and_back_goes_home() {
    let mut runner = reading();
    let commands = runner.action(action_id("read-0"));
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, Command::Store(StoreRequest::ShelfRead { .. }))),
        "a tap only opens the page"
    );
    let screen = last_screen(&commands).expect("a screen");
    let text = screen_text(&screen);
    assert!(
        text.contains("pharos-book-detail") || text.contains("Book One"),
        "{text}"
    );
    assert!(text.contains("Read"), "{text}");
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());
    let commands = runner.action(ActionId::BACK);
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("Continue reading"), "Back goes home: {text}");
}

#[test]
fn reading_from_the_detail_page_closes_back_home() {
    let mut runner = reading();
    let _ = runner.action(action_id("read-0"));
    let commands = runner.action(action_id("book-read"));
    let commands = read_and_close(&mut runner, &commands);
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        text.contains("Continue reading"),
        "closing goes home: {text}"
    );
}

#[test]
fn a_hold_on_a_book_opens_it_where_it_stopped_and_closing_goes_home() {
    let mut runner = reading();
    let commands = runner.action(action_id("read-hold-0"));
    let commands = read_and_close(&mut runner, &commands);
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        text.contains("Continue reading"),
        "closing goes home: {text}"
    );
}

#[test]
fn a_book_never_opened_here_is_not_continued() {
    let (_, commands) = signed_in(Some(record("")));
    assert!(asked(&commands, "Ids=").is_none());
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(!text.contains("Continue reading"), "{text}");
}

#[test]
fn a_full_home_screen_keeps_every_library_on_the_smallest_reader() {
    let many: Vec<String> = (0..8)
        .map(|i| {
            format!(r#"{{"Id":"t{i}","AlbumId":"a{i}","Album":"An album with a long-ish name {i}","UserData":{{"PlaybackPositionTicks":99999}}}}"#)
        })
        .collect();
    let (mut runner, commands) = signed_in(Some(record("at 2\n")));
    let (resume, _) = asked(&commands, "/Items/Resume").expect("resume");
    let mut drawn =
        runner.task_outcome(resume, ok(&format!(r#"{{"Items":[{}]}}"#, many.join(","))));
    let (known, _) = asked(&commands, "Ids=b1&").expect("known");
    drawn.extend(runner.task_outcome(known, ok(BOOKS)));
    let screen = last_screen(&drawn).expect("a screen");
    let text = screen_text(&screen);
    let problems = screen.validate(&CLARA_BW_METRICS);
    assert!(problems.is_empty(), "{problems:?}");
    assert!(text.contains("Continue listening"), "{text}");
    for name in ["Music", "Audiobooks"] {
        assert!(text.contains(name), "{name} missing from {text}");
    }
}

#[test]
fn the_libra_colour_has_room_for_both_rows_and_every_library() {
    let libra = kobo_sdk::DisplayMetrics {
        width: 1264,
        height: 1680,
        pixels_per_inch: 300,
        ..CLARA_BW_METRICS
    };
    let mut runner = AppRunner::with_metrics(Pharos::default(), libra);
    let _ = runner.start();
    let commands = runner.store_result(StoreResult::Loaded {
        key: ACCOUNT_KEY.to_owned(),
        value: Some(account().encode()),
    });
    let (views, _) = asked(&commands, "/Users/u1/Views").expect("views");
    let (resume, _) = asked(&commands, "/Items/Resume").expect("resume");
    let _ = runner.task_outcome(views, ok(VIEWS));
    let commands = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: Some(record("at 2\n")),
    });
    let (known, _) = asked(&commands, "Ids=b1&").expect("known");
    let mut drawn = runner.task_outcome(resume, ok(RESUMED));
    drawn.extend(runner.task_outcome(known, ok(BOOKS)));
    let screen = last_screen(&drawn).expect("a screen");
    let text = screen_text(&screen);
    let problems = screen.validate(&libra);
    assert!(problems.is_empty(), "{problems:?}");
    for part in [
        "Continue listening",
        "Continue reading",
        "Book One",
        "Spoken",
        "Audiobooks",
    ] {
        assert!(text.contains(part), "{part} missing from {text}");
    }
}

#[test]
fn a_home_cover_never_stands_in_on_a_shelf() {
    let (mut runner, commands) = listening();
    let (describe, _) = asked(&commands, "/Users/u1/Items/al1").expect("describe");
    let commands = runner.task_outcome(
        describe,
        ok(r#"{"Id":"al1","Name":"First Album","ImageTags":{"Primary":"c1"}}"#),
    );
    let (cover, _) = cover_tasks(&commands)[0].clone();
    let _ = runner.task_outcome(cover, cover_bytes());
    let commands = runner.action(action_id("section-music"));
    let (albums, _) = asked(&commands, "ParentId=lib-music").expect("albums");
    let commands = runner.task_outcome(
        albums,
        ok(r#"{"Items":[{"Id":"al1","Name":"First Album","ImageTags":{"Primary":"c1"}}],"TotalRecordCount":1}"#),
    );
    assert!(
        cover_tasks(&commands)
            .iter()
            .any(|(_, url)| url.contains("/Items/al1/Images")),
        "the shelf asks for a cover sized to its own tiles"
    );
}
