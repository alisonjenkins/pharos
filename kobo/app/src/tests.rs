use super::*;
use kobo_sdk::{AppRunner, Command, PictureFormat, PictureHandle, StoreRequest, CLARA_BW_METRICS};

/// Every task a batch of commands spawned, with what it asked for.
fn spawns(commands: &[Command]) -> Vec<(TaskId, Task)> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::Spawn { task, work } => Some((*task, work.clone())),
            _ => None,
        })
        .collect()
}

fn url_of(task: &Task) -> Option<&str> {
    match task {
        Task::Fetch { url, .. } | Task::Post { url, .. } => Some(url),
        _ => None,
    }
}

/// The one spawned task whose URL ends with or contains `part`.
fn asked(commands: &[Command], part: &str) -> Option<(TaskId, Task)> {
    spawns(commands)
        .into_iter()
        .find(|(_, task)| url_of(task).is_some_and(|url| url.contains(part)))
}

fn slept(commands: &[Command]) -> Option<TaskId> {
    spawns(commands)
        .into_iter()
        .find_map(|(id, task)| matches!(task, Task::Sleep { .. }).then_some(id))
}

fn last_screen(commands: &[Command]) -> Option<Screen> {
    commands.iter().rev().find_map(|command| match command {
        Command::SetScreen(screen) => Some(screen.clone()),
        _ => None,
    })
}

fn screen_text(screen: &Screen) -> String {
    format!("{screen:?}")
}

fn ok(bytes: &str) -> TaskOutcome {
    TaskOutcome::Completed(bytes.as_bytes().to_vec())
}

fn account() -> Account {
    Account {
        user_id: "u1".to_owned(),
        token: "t0k".to_owned(),
    }
}

/// A runner past start-up, with `saved` as what the store held.
/// A runner whose saved account, if any, has loaded. The opening screen's
/// wait for home is over, so the home screen as far as it got is drawn.
fn started(saved: Option<&Account>) -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, _) = opened();
    let mut commands = runner.store_result(StoreResult::Loaded {
        key: ACCOUNT_KEY.to_owned(),
        value: saved.map(Account::encode),
    });
    if let Some(wait) = slept(&commands) {
        commands.extend(runner.task_outcome(wait, ok("")));
    }
    (runner, commands)
}

/// A runner that has drawn its opening screen, with what it sent.
fn opened() -> (AppRunner<Pharos>, Vec<Command>) {
    let mut runner = AppRunner::new(Pharos::default());
    let commands = runner.start();
    (runner, commands)
}

const VIEWS: &str = r#"{"Items":[
    {"Id":"lib-music","Name":"Songs","CollectionType":"music"},
    {"Id":"lib-books","Name":"Reading","CollectionType":"books"}
]}"#;

/// A signed-in runner whose home screen has loaded its sections, with
/// nothing played. The downloaded-books record is left to the test.
fn home() -> (AppRunner<Pharos>, Vec<Command>) {
    let (runner, commands) = home_with(VIEWS);
    (runner, commands)
}

/// [`home`], with `views` as the libraries pharos lists.
fn home_with(views: &str) -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, commands) = started(Some(&account()));
    let mut all = commands.clone();
    let (asked_views, _) = asked(&commands, "/Users/u1/Views").expect("views");
    all.extend(runner.task_outcome(asked_views, ok(views)));
    let commands = settle_home(&mut runner, all, r#"{"Items":[]}"#, None);
    (runner, commands)
}

/// Answers what the home screen still waits on among `commands`, and what
/// those answers ask for in turn, so it is drawn (B1457): what was played
/// last as `resumed`, each album pharos is asked about as not found, the
/// books read here as `books` when given, and each cover with a picture.
/// Returns `commands` and every command on the way.
fn settle_home(
    runner: &mut AppRunner<Pharos>,
    commands: Vec<Command>,
    resumed: &str,
    books: Option<&str>,
) -> Vec<Command> {
    let mut pending = spawns(&commands);
    let mut all = commands;
    while !pending.is_empty() {
        let (task, work) = pending.remove(0);
        let Some(url) = url_of(&work).map(str::to_owned) else {
            continue;
        };
        let outcome = if url.contains("/Items/Resume") {
            ok(resumed)
        } else if url.contains("/Images/Primary") {
            cover_bytes()
        } else if url.contains("Ids=") {
            match books {
                Some(books) => ok(books),
                None => continue,
            }
        } else if url.contains("/Users/u1/Items/") && !url.contains('?') {
            TaskOutcome::Failed(TaskError::NotFound)
        } else {
            continue;
        };
        let mut reply = runner.task_outcome(task, outcome);
        let cancelled = answer_cancels(runner, &reply);
        reply.extend(cancelled);
        pending.extend(spawns(&reply));
        all.extend(reply);
    }
    all
}

/// Reports each task `commands` cancelled as cancelled, as the runtime
/// does, so its slot is free again. Returns what those reports produced.
fn answer_cancels(runner: &mut AppRunner<Pharos>, commands: &[Command]) -> Vec<Command> {
    let mut all = Vec::new();
    let mut pending: Vec<TaskId> = commands
        .iter()
        .filter_map(|command| match command {
            Command::Cancel(task) => Some(*task),
            _ => None,
        })
        .collect();
    while let Some(task) = pending.pop() {
        let reply = runner.task_outcome(task, TaskOutcome::Cancelled);
        pending.extend(reply.iter().filter_map(|command| match command {
            Command::Cancel(task) => Some(*task),
            _ => None,
        }));
        all.extend(reply);
    }
    all
}

/// A runner on the music shelf, with `ALBUMS` loaded.
fn shelf() -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-music"));
    let (albums, _) = asked(&commands, "ParentId=lib-music").expect("albums");
    let mut commands = runner.task_outcome(albums, ok(ALBUMS));
    let cancelled = answer_cancels(&mut runner, &commands);
    commands.extend(cancelled);
    (runner, commands)
}

const ALBUMS: &str = r#"{"Items":[
    {"Id":"al1","Name":"First Album","AlbumArtist":"Band"},
    {"Id":"al2","Name":"Second Album","AlbumArtist":"Band"}
],"TotalRecordCount":2}"#;

const TRACKS: &str = r#"{"Items":[
    {"Id":"tr1","Name":"Opening","RunTimeTicks":1870000000},
    {"Id":"tr2","Name":"Closing","RunTimeTicks":2400000000}
],"TotalRecordCount":2}"#;

#[test]
fn with_no_saved_account_it_asks_pharos_for_a_code() {
    let (_, commands) = started(None);
    let (_, task) = asked(&commands, "/QuickConnect/Initiate").expect("asked for a code");
    let Task::Post { headers, .. } = task else {
        panic!("initiate is a POST");
    };
    assert!(headers
        .iter()
        .any(|header| header.name == "X-Emby-Authorization" && !header.value.contains("Token")));
}

#[test]
fn quick_connect_signs_in_keeps_the_account_and_asks_for_the_libraries() {
    let (mut runner, commands) = started(None);
    let (initiate, _) = asked(&commands, "/QuickConnect/Initiate").expect("initiate");
    let commands = runner.task_outcome(
        initiate,
        ok(r#"{"Code":"482913","Secret":"sec1","Authenticated":false}"#),
    );
    let screen = last_screen(&commands).expect("a screen");
    assert!(screen_text(&screen).contains("482913"), "the code is shown");

    // Not approved yet: wait, ask, wait again.
    let wait = slept(&commands).expect("waits before polling");
    let commands = runner.task_outcome(wait, ok(""));
    let (poll, _) = asked(&commands, "/QuickConnect/Connect?secret=sec1").expect("polls");
    let commands = runner.task_outcome(poll, ok(r#"{"Authenticated":false}"#));
    let wait = slept(&commands).expect("waits again");

    // Approved: finish, keep the account, list albums with the token.
    let commands = runner.task_outcome(wait, ok(""));
    let (poll, _) = asked(&commands, "/QuickConnect/Connect").expect("polls again");
    let commands = runner.task_outcome(poll, ok(r#"{"Authenticated":true}"#));
    let (finish, task) = asked(&commands, "/Users/AuthenticateWithQuickConnect").expect("finish");
    assert!(matches!(task, Task::Post { ref body, .. } if body.contains("sec1")));
    let commands = runner.task_outcome(finish, ok(r#"{"AccessToken":"t0k","User":{"Id":"u1"}}"#));
    assert!(commands.iter().any(|command| matches!(
        command,
        Command::Store(StoreRequest::Save { key, value }) if key == ACCOUNT_KEY && *value == account().encode()
    )));
    let (_, task) = asked(&commands, "/Users/u1/Views").expect("views");
    let Task::Fetch { headers, .. } = task else {
        panic!("views is a GET");
    };
    assert!(headers
        .iter()
        .any(|header| header.value.contains("Token=\"t0k\"")));
}

#[test]
fn a_saved_account_goes_straight_to_the_libraries() {
    let (_, commands) = started(Some(&account()));
    assert!(asked(&commands, "/QuickConnect").is_none());
    assert!(asked(&commands, "/Users/u1/Views").is_some());
}

#[test]
fn a_rejected_token_is_forgotten_and_sign_in_starts_again() {
    let (mut runner, commands) = started(Some(&account()));
    let (views, _) = asked(&commands, "/Users/u1/Views").expect("views");
    let commands = runner.task_outcome(views, TaskOutcome::Failed(TaskError::Unauthorized));
    assert!(commands.iter().any(|command| matches!(
        command,
        Command::Store(StoreRequest::Forget { key }) if key == ACCOUNT_KEY
    )));
    assert!(asked(&commands, "/QuickConnect/Initiate").is_some());
}

#[test]
fn an_expired_code_is_replaced() {
    let (mut runner, commands) = started(None);
    let (initiate, _) = asked(&commands, "Initiate").expect("initiate");
    let commands = runner.task_outcome(initiate, ok(r#"{"Code":"1","Secret":"s"}"#));
    let commands = runner.task_outcome(slept(&commands).expect("wait"), ok(""));
    let (poll, _) = asked(&commands, "Connect").expect("poll");
    let commands = runner.task_outcome(poll, TaskOutcome::Failed(TaskError::NotFound));
    assert!(asked(&commands, "/QuickConnect/Initiate").is_some());
}

/// Opens the first album and plays its first track.
fn playing() -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, _) = shelf();
    let commands = runner.action(action_id("tile-0"));
    let (tracks, _) = asked(&commands, "ParentId=al1").expect("tracks");
    let _ = runner.task_outcome(tracks, ok(TRACKS));
    let commands = runner.action(action_id("row-1"));
    (runner, commands)
}

#[test]
fn playing_a_track_tells_pharos_where_the_listener_is() {
    let (_, commands) = playing();
    let (_, task) = asked(&commands, "/Sessions/Playing").expect("started report");
    let Task::Post { url, body, .. } = task else {
        panic!("a report is a POST");
    };
    assert!(url.ends_with("/Sessions/Playing"));
    assert!(body.contains("\"ItemId\":\"tr2\""), "{body}");
    assert!(
        slept(&commands).is_some(),
        "a progress timer runs while playing"
    );
}

#[test]
fn leaving_the_player_keeps_playing_and_backs_out_to_the_album() {
    let (mut runner, _) = playing();
    let commands = runner.action(ActionId::BACK);
    let screen = last_screen(&commands).expect("a screen");
    assert!(screen_text(&screen).contains("First Album"));
    assert!(asked(&commands, "/Sessions/Playing/Stopped").is_none());
}

#[test]
fn signing_out_stops_playback_and_forgets_the_account() {
    let (mut runner, _) = playing();
    let _ = runner.action(ActionId::BACK);
    let _ = runner.action(ActionId::BACK);
    let _ = runner.action(ActionId::BACK);
    let _ = runner.action(action_id("home-menu"));
    let commands = runner.action(action_id("sign-out"));
    assert!(asked(&commands, "/Sessions/Playing/Stopped").is_some());
    assert!(commands.iter().any(|command| matches!(
        command,
        Command::Store(StoreRequest::Forget { key }) if key == ACCOUNT_KEY
    )));
}

#[test]
fn sign_out_is_behind_the_home_menu() {
    let (mut runner, commands) = home();
    let screen = last_screen(&commands).expect("a home screen");
    assert!(!screen_text(&screen).contains("Sign out"));

    let commands = runner.action(action_id("home-menu"));
    let screen = last_screen(&commands).expect("the menu");
    assert!(screen_text(&screen).contains("Sign out"));

    let commands = runner.action(ActionId::BACK);
    let screen = last_screen(&commands).expect("home again");
    assert!(!screen_text(&screen).contains("Sign out"));
    assert!(screen_text(&screen).contains("Music"));
    assert!(!commands.iter().any(|command| matches!(
        command,
        Command::Store(StoreRequest::Forget { key }) if key == ACCOUNT_KEY
    )));
}

#[test]
fn every_screen_fits_the_smallest_reader() {
    let mut screens = Vec::new();
    let (mut runner, commands) = started(None);
    let (initiate, _) = asked(&commands, "Initiate").expect("initiate");
    screens.extend(last_screen(&commands));
    let commands = runner.task_outcome(initiate, ok(r#"{"Code":"482913","Secret":"s"}"#));
    screens.extend(last_screen(&commands));

    let many: String = (0..20)
        .map(|index| {
            format!(r#"{{"Id":"a{index}","Name":"An album with a long-ish name {index}","AlbumArtist":"Someone"}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    let (mut runner, commands) = home();
    screens.extend(last_screen(&commands));
    let commands = runner.action(action_id("section-music"));
    let (albums, _) = asked(&commands, "ParentId=lib-music").expect("albums");
    let commands = runner.task_outcome(
        albums,
        ok(&format!(r#"{{"Items":[{many}],"TotalRecordCount":20}}"#)),
    );
    screens.extend(last_screen(&commands));
    let commands = runner.action(action_id("tile-0"));
    screens.extend(last_screen(&commands));
    let (tracks, _) = asked(&commands, "ParentId=a0").expect("tracks");
    let commands = runner.task_outcome(tracks, ok(TRACKS));
    screens.extend(last_screen(&commands));

    assert_eq!(screens.len(), 6);
    for (index, screen) in screens.iter().enumerate() {
        let problems = screen.validate(&CLARA_BW_METRICS);
        assert!(problems.is_empty(), "screen {index}: {problems:?}");
    }
}

#[test]
fn tracks_for_an_album_already_left_are_ignored() {
    let (mut runner, _) = shelf();
    let commands = runner.action(action_id("tile-0"));
    let (first, _) = asked(&commands, "ParentId=al1").expect("first album's tracks");
    let _ = runner.action(ActionId::BACK);
    let commands = runner.action(action_id("tile-1"));
    let (second, _) = asked(&commands, "ParentId=al2").expect("second album's tracks");
    let _ = runner.task_outcome(
        second,
        ok(r#"{"Items":[{"Id":"b1","Name":"Second's track"}],"TotalRecordCount":1}"#),
    );
    let _ = runner.task_outcome(first, ok(TRACKS));
    // The stale reply changed nothing: the first row is still the second album's.
    let commands = runner.action(action_id("row-0"));
    let (_, task) = asked(&commands, "/Sessions/Playing").expect("started report");
    let Task::Post { body, .. } = task else {
        panic!("a report is a POST");
    };
    assert!(body.contains("\"ItemId\":\"b1\""), "{body}");
}

#[test]
fn an_album_longer_than_a_page_loads_all_its_tracks() {
    let (mut runner, _) = shelf();
    let commands = runner.action(action_id("tile-0"));
    let (first, _) = asked(&commands, "StartIndex=0&").expect("first block");
    let items: String = (0..50)
        .map(|index| format!(r#"{{"Id":"t{index}","Name":"Track {index}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let commands = runner.task_outcome(
        first,
        ok(&format!(r#"{{"Items":[{items}],"TotalRecordCount":51}}"#)),
    );
    let (rest, _) = asked(&commands, "StartIndex=50&").expect("the rest");
    let commands = runner.task_outcome(
        rest,
        ok(r#"{"Items":[{"Id":"t50","Name":"Track 50"}],"TotalRecordCount":51}"#),
    );
    assert!(
        asked(&commands, "ParentId=al1").is_none(),
        "nothing left to fetch"
    );
}

#[test]
fn a_rejected_report_signs_in_again() {
    let (mut runner, commands) = playing();
    let (report, _) = asked(&commands, "/Sessions/Playing").expect("started report");
    let commands = runner.task_outcome(report, TaskOutcome::Failed(TaskError::Unauthorized));
    assert!(commands.iter().any(|command| matches!(
        command,
        Command::Store(StoreRequest::Forget { key }) if key == ACCOUNT_KEY
    )));
    assert!(asked(&commands, "/QuickConnect/Initiate").is_some());
}

#[test]
fn the_album_offers_a_way_back_to_the_player() {
    let (mut runner, _) = playing();
    let commands = runner.action(ActionId::BACK);
    let album = last_screen(&commands).expect("a screen");
    let problems = album.validate(&CLARA_BW_METRICS);
    assert!(problems.is_empty(), "{problems:?}");
    let screen = screen_text(&album);
    assert!(screen.contains("Playing"), "{screen}");
    let commands = runner.action(action_id("now-playing"));
    assert!(
        asked(&commands, "/Sessions/Playing").is_none(),
        "no restart"
    );
    let commands = runner.action(ActionId::BACK);
    assert!(screen_text(&last_screen(&commands).expect("a screen")).contains("First Album"));
}

#[test]
fn a_later_block_of_tracks_failing_leaves_the_player_alone() {
    let (mut runner, _) = shelf();
    let commands = runner.action(action_id("tile-0"));
    let (first, _) = asked(&commands, "StartIndex=0&").expect("first block");
    let commands = runner.task_outcome(
        first,
        ok(r#"{"Items":[{"Id":"t0","Name":"Track 0"}],"TotalRecordCount":2}"#),
    );
    let (rest, _) = asked(&commands, "StartIndex=1&").expect("the rest");
    let _ = runner.action(action_id("row-0"));
    let commands = runner.task_outcome(rest, TaskOutcome::Failed(TaskError::Offline));
    assert!(
        last_screen(&commands).is_none_or(|screen| !screen_text(&screen).contains("offline")),
        "the player stays up"
    );
}

#[test]
fn a_loading_album_still_offers_the_player() {
    let (mut runner, _) = playing();
    let _ = runner.action(ActionId::BACK);
    let _ = runner.action(ActionId::BACK);
    let commands = runner.action(action_id("tile-1"));
    assert!(
        asked(&commands, "ParentId=al2").is_some(),
        "the second album loads"
    );
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("Loading the tracks"), "{screen}");
    assert!(screen.contains("Playing"), "{screen}");
}

#[test]
fn the_home_screen_offers_a_section_for_each_kind_of_library() {
    let (_, commands) = home();
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    for name in ["Music", "Audiobooks", "Books"] {
        assert!(screen.contains(name), "{name} missing from {screen}");
    }
}

#[test]
fn an_account_with_no_libraries_is_told_so() {
    let (_, commands) = home_with(r#"{"Items":[]}"#);
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("shows no music"), "{screen}");
}

#[test]
fn the_audiobooks_section_keeps_only_audiobooks_and_music_only_music() {
    let mixed = r#"{"Items":[
        {"Id":"al1","Name":"Album One","AlbumArtist":"Artist A"},
        {"Id":"bk1","Name":"Book One","AlbumArtist":"Author A","PharosAudiobook":true}
    ],"TotalRecordCount":2}"#;
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-audiobooks"));
    let (albums, _) = asked(&commands, "ParentId=lib-books").expect("albums");
    let commands = runner.task_outcome(albums, ok(mixed));
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        screen.contains("Book One") && !screen.contains("Album One"),
        "{screen}"
    );

    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-music"));
    let (albums, _) = asked(&commands, "ParentId=lib-music").expect("albums");
    let commands = runner.task_outcome(albums, ok(mixed));
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(
        screen.contains("Album One") && !screen.contains("Book One"),
        "{screen}"
    );
}

#[test]
fn a_shelf_loads_every_block_even_when_a_block_keeps_nothing() {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-audiobooks"));
    let (first, _) = asked(&commands, "StartIndex=0&").expect("first block");
    let commands = runner.task_outcome(
        first,
        ok(r#"{"Items":[{"Id":"a","Name":"Album One"}],"TotalRecordCount":2}"#),
    );
    let (second, _) = asked(&commands, "StartIndex=1&").expect("the next block is asked for");
    let commands = runner.task_outcome(
        second,
        ok(r#"{"Items":[{"Id":"b","Name":"Book One","PharosAudiobook":true}],"TotalRecordCount":2}"#),
    );
    assert!(screen_text(&last_screen(&commands).expect("a screen")).contains("Book One"));
}

#[test]
fn a_shelf_block_for_a_shelf_already_left_is_ignored() {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-music"));
    let (stale, _) = asked(&commands, "ParentId=lib-music").expect("music");
    let _ = runner.action(ActionId::BACK);
    let commands = runner.action(action_id("section-audiobooks"));
    let (fresh, _) = asked(&commands, "ParentId=lib-books").expect("audiobooks");
    let _ = runner.task_outcome(stale, ok(ALBUMS));
    let commands = runner.task_outcome(fresh, ok(r#"{"Items":[],"TotalRecordCount":0}"#));
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(!screen.contains("First Album"), "{screen}");
}

/// `count` albums, each with a cover tag, named `Album n`.
fn albums_with_art(count: usize) -> String {
    let items: Vec<String> = (0..count)
        .map(|index| {
            format!(
                r#"{{"Id":"a{index}","Name":"Album {index}","AlbumArtist":"Artist A","ImageTags":{{"Primary":"t{index}"}}}}"#
            )
        })
        .collect();
    format!(
        r#"{{"Items":[{}],"TotalRecordCount":{count}}}"#,
        items.join(",")
    )
}

/// A small picture, as the JPEG or PNG a cover reply would carry.
fn cover_bytes() -> TaskOutcome {
    let rgb: Vec<u8> = (0u32..64 * 96)
        .flat_map(|n| [u8::try_from(n % 251).unwrap_or(0), 90, 200])
        .collect();
    TaskOutcome::Completed(kobo_image::encode_png_rgb(64, 96, &rgb).expect("a png"))
}

/// A runner on the music shelf holding `count` albums with covers, and the
/// commands the shelf produced.
fn art_shelf(colour: bool, count: usize) -> (AppRunner<Pharos>, Vec<Command>) {
    let mut runner = settled_home(colour);
    let commands = runner.action(action_id("section-music"));
    let (albums, _) = asked(&commands, "ParentId=lib-music").expect("albums");
    let commands = runner.task_outcome(albums, ok(&albums_with_art(count)));
    (runner, commands)
}

/// A runner on a home screen that has everything it shows.
fn settled_home(colour: bool) -> AppRunner<Pharos> {
    let mut runner = AppRunner::new(Pharos::default()).showing_colour(colour);
    let _ = runner.start();
    let commands = runner.store_result(StoreResult::Loaded {
        key: ACCOUNT_KEY.to_owned(),
        value: Some(account().encode()),
    });
    let wait = slept(&commands).expect("the opening screen waits for home");
    let _ = runner.task_outcome(wait, ok(""));
    let (views, _) = asked(&commands, "/Users/u1/Views").expect("views");
    let mut all = commands;
    all.extend(runner.task_outcome(views, ok(VIEWS)));
    let _ = settle_home(&mut runner, all, r#"{"Items":[]}"#, None);
    runner
}

fn cover_tasks(commands: &[Command]) -> Vec<(TaskId, String)> {
    spawns(commands)
        .into_iter()
        .filter_map(|(id, task)| {
            let url = url_of(&task)?;
            url.contains("/Images/Primary")
                .then(|| (id, url.to_owned()))
        })
        .collect()
}

fn puts(commands: &[Command]) -> Vec<(PictureHandle, PictureFormat, u32, u32)> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::PutPicture {
                handle,
                format,
                width,
                height,
                ..
            } => Some((*handle, *format, *width, *height)),
            _ => None,
        })
        .collect()
}

fn drops(commands: &[Command]) -> Vec<PictureHandle> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::DropPicture(handle) => Some(*handle),
            _ => None,
        })
        .collect()
}

#[test]
fn the_shelf_asks_for_covers_with_the_sign_in_and_a_bounded_few_at_once() {
    let (_, commands) = art_shelf(false, 40);
    let covers = cover_tasks(&commands);
    assert_eq!(covers.len(), MAX_COVER_TASKS, "two at a time, no more");
    let (_, url) = &covers[0];
    assert!(url.starts_with(&format!("{SERVER}/Items/a0/Images/Primary?fillWidth=")));
    assert!(url.contains("&quality=80&format=jpg&tag=t0"), "{url}");
    let (_, task) = asked(&commands, "/Items/a0/Images/Primary").expect("first cover");
    let Task::Fetch { headers, .. } = task else {
        panic!("a cover is a GET");
    };
    assert!(headers
        .iter()
        .any(|header| header.name == "X-Emby-Authorization"
            && header.value.contains("Token=\"t0k\"")));
}

#[test]
fn an_album_with_no_cover_tag_is_not_asked_for_one() {
    let (_, commands) = shelf();
    assert!(cover_tasks(&commands).is_empty());
}

#[test]
fn a_cover_is_sent_grey_to_a_grey_panel_and_in_colour_to_a_colour_one() {
    let (mut runner, commands) = art_shelf(false, 3);
    let (first, _) = cover_tasks(&commands)[0].clone();
    let commands = runner.task_outcome(first, cover_bytes());
    let sent = puts(&commands);
    assert_eq!(sent.len(), 1, "{commands:?}");
    assert_eq!(sent[0].1, PictureFormat::Grey);

    let (mut runner, commands) = art_shelf(true, 3);
    let (first, _) = cover_tasks(&commands)[0].clone();
    let commands = runner.task_outcome(first, cover_bytes());
    let sent = puts(&commands);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, PictureFormat::Rgb);
    // The picture is fitted to its tile, so it is not the size it arrived at.
    assert_ne!((sent[0].2, sent[0].3), (64, 96));
}

#[test]
fn a_square_cover_on_a_tall_tile_is_fitted_whole_not_cropped() {
    let (mut runner, commands) = books_shelf(false);
    let (first, _) = cover_tasks(&commands)[0].clone();
    let rgb: Vec<u8> = (0u32..90 * 90)
        .flat_map(|n| [u8::try_from(n % 251).unwrap_or(0), 90, 200])
        .collect();
    let square = TaskOutcome::Completed(kobo_image::encode_png_rgb(90, 90, &rgb).expect("a png"));
    let sent = puts(&runner.task_outcome(first, square));
    assert_eq!(sent.len(), 1);
    let (_, _, width, height) = sent[0];
    assert_eq!(
        width, height,
        "the whole square cover is kept: {width}x{height}"
    );
}

/// Answers covers, first asked first, until the page is drawn with them.
fn page_drawn(runner: &mut AppRunner<Pharos>, commands: &[Command]) -> Screen {
    let mut pending = cover_tasks(commands);
    while !pending.is_empty() {
        let (task, _) = pending.remove(0);
        let reply = runner.task_outcome(task, cover_bytes());
        if let Some(screen) = last_screen(&reply) {
            return screen;
        }
        pending.extend(cover_tasks(&reply));
    }
    panic!("the page was never drawn with its covers");
}

#[test]
fn an_arrived_cover_replaces_the_glyph_on_its_tile() {
    let (mut runner, commands) = art_shelf(false, 3);
    assert!(
        last_screen(&commands).is_none(),
        "the page is not drawn bare before its covers (B1458)"
    );
    let after = screen_text(&page_drawn(&mut runner, &commands));
    assert!(after.contains("picture: Some"), "{after}");
    assert!(!after.contains("picture: None"), "{after}");
}

#[test]
fn a_cover_for_a_shelf_already_left_is_dropped_and_its_reply_ignored() {
    let (mut runner, commands) = art_shelf(false, 3);
    let covers = cover_tasks(&commands);
    let commands = runner.action(ActionId::BACK);
    for (task, _) in &covers {
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, Command::Cancel(id) if id == task)),
            "the cover on its way is cancelled"
        );
    }
    let commands = runner.task_outcome(covers[0].0, cover_bytes());
    assert!(puts(&commands).is_empty(), "a stale reply is not sent on");
}

#[test]
fn opening_an_album_keeps_the_shelf_covers_coming_and_back_finds_them() {
    let (mut runner, commands) = art_shelf(false, 6);
    let covers = cover_tasks(&commands);
    let commands = runner.action(action_id("tile-1"));
    for (task, _) in &covers {
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, Command::Cancel(id) if id == task)),
            "a shelf cover on its way is not cancelled by opening an album: {commands:?}"
        );
    }
    // The album's own cover is not asked twice: the shelf already has it coming.
    let mut pending = covers;
    pending.extend(cover_tasks(&commands));
    while !pending.is_empty() {
        let (task, _) = pending.remove(0);
        let reply = runner.task_outcome(task, cover_bytes());
        pending.extend(cover_tasks(&reply));
    }
    let commands = runner.action(ActionId::BACK);
    assert!(
        cover_tasks(&commands).is_empty(),
        "Back draws the covers already in, asking for none again"
    );
    let text = screen_text(&last_screen(&commands).expect("the shelf"));
    assert!(!text.contains("picture: None"), "{text}");
}

#[test]
fn a_cover_that_fails_keeps_its_glyph_is_not_asked_again_and_does_not_end_the_session() {
    let (mut runner, commands) = art_shelf(false, 6);
    let (first, _) = cover_tasks(&commands)[0].clone();
    let commands = runner.task_outcome(first, TaskOutcome::Failed(TaskError::NotFound));
    assert!(
        last_screen(&commands).is_none_or(|screen| {
            let text = screen_text(&screen);
            text.contains("Album 0") && !text.contains("could not be reached")
        }),
        "the shelf stays up"
    );
    assert!(
        cover_tasks(&commands)
            .iter()
            .all(|(_, url)| !url.contains("/a0/")),
        "a failed cover is not retried"
    );
    let logged = commands
        .iter()
        .filter(|command| matches!(command, Command::Log { message, .. } if message.contains("cover a0")))
        .count();
    assert_eq!(logged, 1);
    // Not even a refused sign-in on a cover signs the reader out.
    let (second, _) = cover_tasks(&commands)
        .into_iter()
        .next()
        .expect("the next cover starts");
    let commands = runner.task_outcome(second, TaskOutcome::Failed(TaskError::Unauthorized));
    assert!(asked(&commands, "/QuickConnect/Initiate").is_none());
    // And bytes that are no picture fail the same way.
    let (third, _) = cover_tasks(&commands)
        .into_iter()
        .next()
        .expect("another cover starts");
    let commands = runner.task_outcome(third, ok("not an image"));
    assert!(puts(&commands).is_empty());
}

/// The seconds of each sleep `commands` started.
fn sleeps(commands: &[Command]) -> Vec<(TaskId, u32)> {
    spawns(commands)
        .into_iter()
        .filter_map(|(id, task)| match task {
            Task::Sleep { seconds } => Some((id, seconds)),
            _ => None,
        })
        .collect()
}

/// A two-cover shelf whose second cover arrives and whose first fails with
/// `error`. Returns the runner, the first cover's task id and the commands
/// the failure produced after the second landed.
fn shelf_with_one_failed_cover(error: TaskError) -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, commands) = art_shelf(false, 2);
    let covers = cover_tasks(&commands);
    assert_eq!(covers.len(), 2);
    let mut all = runner.task_outcome(covers[0].0, TaskOutcome::Failed(error));
    all.extend(runner.task_outcome(covers[1].0, cover_bytes()));
    (runner, all)
}

#[test]
fn a_cover_that_fails_unreachable_is_asked_again_after_a_wait_and_draws_once() {
    let (mut runner, commands) = shelf_with_one_failed_cover(TaskError::Unreachable);
    assert!(
        logged(&commands, "pharos: cover a0 retry in 15s: Unreachable"),
        "{commands:?}"
    );
    assert_eq!(draws(&commands), 1, "the page drew once with a glyph");
    assert!(cover_tasks(&commands).is_empty(), "not asked at once");
    let (nap, seconds) = *sleeps(&commands)
        .iter()
        .find(|(_, seconds)| *seconds == 15)
        .expect("one sleep counts down to the retry");
    assert_eq!(seconds, 15);
    let woke = runner.task_outcome(nap, ok(""));
    let again = cover_tasks(&woke);
    assert_eq!(again.len(), 1, "{woke:?}");
    assert!(again[0].1.contains("/a0/"), "the failed cover is asked");
    let landed = runner.task_outcome(again[0].0, cover_bytes());
    assert_eq!(draws(&landed), 1, "the retried cover folds into one draw");
    let text = screen_text(&last_screen(&landed).expect("the page"));
    assert!(!text.contains("picture: None"), "{text}");
}

#[test]
fn a_cover_that_keeps_failing_waits_longer_each_time_up_to_five_minutes() {
    let (mut runner, commands) = shelf_with_one_failed_cover(TaskError::TimedOut);
    let mut nap = sleeps(&commands)[0];
    let mut waits = vec![nap.1];
    for _ in 0..7 {
        let woke = runner.task_outcome(nap.0, ok(""));
        let (task, _) = cover_tasks(&woke)[0].clone();
        let failed = runner.task_outcome(task, TaskOutcome::Failed(TaskError::Unreachable));
        nap = sleeps(&failed)[0];
        waits.push(nap.1);
    }
    assert_eq!(waits, [15, 30, 60, 120, 240, 300, 300, 300]);
}

#[test]
fn a_cover_that_fails_during_a_sleep_still_waits_its_full_delay() {
    let (mut runner, commands) = art_shelf(false, 3);
    let covers = cover_tasks(&commands);
    let first = runner.task_outcome(covers[0].0, TaskOutcome::Failed(TaskError::Unreachable));
    let (nap, seconds) = sleeps(&first)[0];
    assert_eq!(seconds, 15);
    // The second fails while the first's sleep runs: it is timed from that
    // sleep's end, not from a clock the sleep has not advanced yet.
    let second = runner.task_outcome(covers[1].0, TaskOutcome::Failed(TaskError::Unreachable));
    assert!(
        logged(&second, "pharos: cover a1 retry in 15s"),
        "{second:?}"
    );
    // The third cover lands, which frees a slot for the retry.
    for (task, _) in cover_tasks(&first).iter().chain(&cover_tasks(&second)) {
        let _ = runner.task_outcome(*task, cover_bytes());
    }
    let woke = runner.task_outcome(nap, ok(""));
    let again = cover_tasks(&woke);
    assert_eq!(again.len(), 1, "only the first is due: {woke:?}");
    assert!(again[0].1.contains("/a0/"), "{again:?}");
    let (nap, seconds) = sleeps(&woke)[0];
    assert_eq!(seconds, 15, "the second waits its own full delay");
    let woke = runner.task_outcome(nap, ok(""));
    let again = cover_tasks(&woke);
    assert_eq!(again.len(), 1, "{woke:?}");
    assert!(again[0].1.contains("/a1/"), "{again:?}");
}

#[test]
fn leaving_the_page_cancels_the_retry_sleep_and_frees_its_slot() {
    let (mut runner, commands) = art_shelf(false, 2);
    let covers = cover_tasks(&commands);
    let failed = runner.task_outcome(covers[0].0, TaskOutcome::Failed(TaskError::Unreachable));
    let (nap, _) = sleeps(&failed)[0];
    let _ = runner.task_outcome(covers[1].0, cover_bytes());
    let left = runner.action(ActionId::BACK);
    assert!(
        left.iter()
            .any(|command| matches!(command, Command::Cancel(id) if *id == nap)),
        "{left:?}"
    );
    assert!(logged(
        &left,
        "pharos: cover retry sleep cancelled: no cover waits"
    ));
    assert!(sleeps(&left).is_empty(), "nothing waits: {left:?}");
    let after = answer_cancels(&mut runner, &left);
    assert!(sleeps(&after).is_empty(), "{after:?}");
    assert!(cover_tasks(&after).is_empty(), "{after:?}");
}

#[test]
fn a_cover_due_sooner_replaces_a_longer_retry_sleep() {
    let (mut runner, commands) = art_shelf(false, 20);
    let covers = cover_tasks(&commands);
    // A cover of page one fails and waits 15 s.
    let first = runner.task_outcome(covers[0].0, TaskOutcome::Failed(TaskError::Unreachable));
    let (nap, _) = sleeps(&first)[0];
    for (task, _) in &covers[1..] {
        let _ = runner.task_outcome(*task, cover_bytes());
    }
    // Page two wants nothing waiting, so the sleep goes.
    let next = runner.action(action_id("page-next"));
    assert!(
        next.iter()
            .any(|command| matches!(command, Command::Cancel(id) if *id == nap)),
        "{next:?}"
    );
    let mut freed = answer_cancels(&mut runner, &next);
    // The page's own draw wait is over, which frees its slot for covers.
    let (wait, _) = sleeps(&next)[0];
    freed.extend(runner.task_outcome(wait, ok("")));
    // A cover there is told to wait five minutes.
    let page_two = cover_tasks(&freed);
    assert!(!page_two.is_empty(), "page two asks for covers: {freed:?}");
    let failed = runner.task_outcome(
        page_two[0].0,
        TaskOutcome::Failed(TaskError::RateLimited(300)),
    );
    let (long, seconds) = sleeps(&failed)[0];
    assert_eq!(seconds, 300);
    // Back on page one, its cover is due much sooner.
    let back = runner.action(action_id("page-back"));
    assert!(
        back.iter()
            .any(|command| matches!(command, Command::Cancel(id) if *id == long)),
        "{back:?}"
    );
    assert!(logged(
        &back,
        "pharos: cover retry sleep cancelled: a cover is due sooner"
    ));
    let (_, seconds) = sleeps(&back)[0];
    assert_eq!(seconds, 15);
    // The answer to the cancel moves no clock and starts nothing.
    let after = answer_cancels(&mut runner, &back);
    assert!(sleeps(&after).is_empty(), "{after:?}");
    assert!(cover_tasks(&after).is_empty(), "{after:?}");
}

#[test]
fn a_cover_the_server_will_not_give_is_never_asked_again() {
    for error in [
        TaskError::NotFound,
        TaskError::Unauthorized,
        TaskError::TooLarge,
        TaskError::Denied,
    ] {
        let (_, commands) = shelf_with_one_failed_cover(error);
        assert!(logged(&commands, "pharos: cover a0 skipped"), "{error:?}");
        assert!(!logged(&commands, "retry in"), "{error:?}");
        assert!(sleeps(&commands).is_empty(), "{error:?}: {commands:?}");
    }
}

#[test]
fn a_cover_waiting_to_retry_does_not_hold_the_page_draw() {
    let (mut runner, commands) = art_shelf(false, 2);
    let covers = cover_tasks(&commands);
    let first = runner.task_outcome(covers[0].0, TaskOutcome::Failed(TaskError::Unreachable));
    assert_eq!(draws(&first), 0, "the other cover is still on its way");
    assert!(logged(&first, "covers_pending=true"), "{first:?}");
    let second = runner.task_outcome(covers[1].0, cover_bytes());
    assert_eq!(draws(&second), 1, "only a waiting cover is left, so draw");
    assert!(logged(&second, "covers_pending=false"), "{second:?}");
}

#[test]
fn paging_through_hundreds_of_albums_keeps_the_held_covers_bounded() {
    let (mut runner, mut commands) = art_shelf(false, 120);
    let mut held: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut most = 0;
    let mut dropped_any = false;
    for _ in 0..14 {
        // Answer every cover on its way until none is.
        while !cover_tasks(&commands).is_empty() {
            let mut next = Vec::new();
            for (task, _) in cover_tasks(&commands) {
                let reply = runner.task_outcome(task, cover_bytes());
                held.extend(puts(&reply).iter().map(|put| put.0 .0));
                for dropped in drops(&reply) {
                    dropped_any = true;
                    held.remove(&dropped.0);
                }
                next.extend(reply);
            }
            commands = next;
        }
        most = most.max(held.len());
        commands = runner.action(action_id("page-next"));
        for dropped in drops(&commands) {
            dropped_any = true;
            held.remove(&dropped.0);
        }
    }
    assert!(dropped_any, "old covers were let go");
    assert!(most <= 40, "held {most} covers at once");
    assert!(most >= 9, "a page of covers was held, got {most}");
}

#[test]
fn the_shelf_screens_fit_the_smallest_reader_with_and_without_covers() {
    let (mut runner, commands) = art_shelf(true, 40);
    let screen = page_drawn(&mut runner, &commands);
    let problems = screen.validate(&CLARA_BW_METRICS);
    assert!(problems.is_empty(), "{problems:?}");
    // Every cover failing draws the page with its glyphs.
    let (mut runner, mut commands) = art_shelf(true, 40);
    let mut screen = None;
    while let Some((task, _)) = cover_tasks(&commands).first().cloned() {
        let reply = runner.task_outcome(task, TaskOutcome::Failed(TaskError::NotFound));
        screen = last_screen(&reply).or(screen);
        commands.retain(
            |command| !matches!(command, Command::Spawn { task: held, .. } if *held == task),
        );
        commands.extend(reply);
    }
    let screen = screen.expect("the page drawn without covers");
    assert!(!screen_text(&screen).contains("picture: Some"));
    let problems = screen.validate(&CLARA_BW_METRICS);
    assert!(problems.is_empty(), "{problems:?}");
}

const BOOKS: &str = r#"{"Items":[
    {"Id":"bk1","Name":"Book One","AlbumArtist":"Author A","PharosAudiobook":true,
     "ProductionYear":2001,"RunTimeTicks":37250000000,"ImageTags":{"Primary":"t1"}}
],"TotalRecordCount":1}"#;

/// The audiobooks shelf, loaded.
fn books_shelf(colour: bool) -> (AppRunner<Pharos>, Vec<Command>) {
    let mut runner = AppRunner::new(Pharos::default()).showing_colour(colour);
    let _ = runner.start();
    let commands = runner.store_result(StoreResult::Loaded {
        key: ACCOUNT_KEY.to_owned(),
        value: Some(account().encode()),
    });
    let (views, _) = asked(&commands, "/Users/u1/Views").expect("views");
    let mut all = commands;
    all.extend(runner.task_outcome(views, ok(VIEWS)));
    let _ = settle_home(&mut runner, all, r#"{"Items":[]}"#, None);
    let commands = runner.action(action_id("section-audiobooks"));
    let (books, _) = asked(&commands, "ParentId=lib-books").expect("books");
    let commands = runner.task_outcome(books, ok(BOOKS));
    (runner, commands)
}

#[test]
fn an_album_opens_on_a_masthead_with_its_artist_year_and_length() {
    let (mut runner, _) = books_shelf(false);
    let commands = runner.action(action_id("tile-0"));
    let screen = last_screen(&commands).expect("a screen");
    let text = screen_text(&screen);
    for part in ["Book One", "Author A", "2001", "1:02:05"] {
        assert!(text.contains(part), "{part} missing from {text}");
    }
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());
}

#[test]
fn the_masthead_shows_the_cover_already_held_or_the_one_that_arrives() {
    let (mut runner, commands) = books_shelf(false);
    let (cover, _) = cover_tasks(&commands)[0].clone();
    // Opened while the cover is still on its way: no picture yet, no new fetch.
    let commands = runner.action(action_id("tile-0"));
    assert!(!screen_text(&last_screen(&commands).expect("a screen")).contains("Picture"));
    assert!(
        cover_tasks(&commands).is_empty(),
        "the one on its way serves"
    );
    let commands = runner.task_outcome(cover, cover_bytes());
    let screen = last_screen(&commands).expect("a screen");
    assert!(screen_text(&screen).contains("Picture"), "{screen:?}");
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());
    // And with the cover held, opening again needs no fetch.
    let _ = runner.action(ActionId::BACK);
    let commands = runner.action(action_id("tile-0"));
    assert!(cover_tasks(&commands).is_empty());
    assert!(screen_text(&last_screen(&commands).expect("a screen")).contains("Picture"));
}

#[test]
fn a_long_album_pages_under_the_masthead_and_every_page_fits() {
    let (mut runner, _) = books_shelf(true);
    let commands = runner.action(action_id("tile-0"));
    let (tracks, _) = asked(&commands, "ParentId=bk1").expect("tracks");
    let items: Vec<String> = (0..30)
        .map(|index| {
            format!(
                r#"{{"Id":"c{index}","Name":"Chapter {index} with a longer name","RunTimeTicks":18700000000}}"#
            )
        })
        .collect();
    let commands = runner.task_outcome(
        tracks,
        ok(&format!(
            r#"{{"Items":[{}],"TotalRecordCount":30}}"#,
            items.join(",")
        )),
    );
    let first = last_screen(&commands).expect("a screen");
    assert!(screen_text(&first).contains("Author A"));
    assert!(screen_text(&first).contains("Chapter 0"));
    assert!(first.validate(&CLARA_BW_METRICS).is_empty());
    let mut seen = std::collections::BTreeSet::new();
    let mut last = screen_text(&first);
    for _ in 0..12 {
        let commands = runner.action(action_id("page-next"));
        let Some(screen) = last_screen(&commands) else {
            break;
        };
        let problems = screen.validate(&CLARA_BW_METRICS);
        assert!(problems.is_empty(), "{problems:?}");
        last = screen_text(&screen);
        for index in 0..30 {
            if last.contains(&format!("Chapter {index} with")) {
                seen.insert(index);
            }
        }
    }
    assert!(last.contains("Chapter 29"), "the last page is reached");
    assert!(!last.contains("Author A"), "later pages carry no masthead");
    assert!(seen.len() >= 20, "{seen:?}");
}

#[test]
fn playing_from_an_album_with_a_masthead_still_starts_the_chosen_track() {
    let (_, commands) = playing();
    let (_, task) = asked(&commands, "/Sessions/Playing").expect("started report");
    let Task::Post { body, .. } = task else {
        panic!("a report is a POST");
    };
    assert!(body.contains("\"ItemId\":\"tr2\""), "{body}");
}

#[test]
fn the_player_shows_the_album_cover_when_it_is_held() {
    let (mut runner, commands) = books_shelf(false);
    let (cover, _) = cover_tasks(&commands)[0].clone();
    let _ = runner.task_outcome(cover, cover_bytes());
    let commands = runner.action(action_id("tile-0"));
    let (tracks, _) = asked(&commands, "ParentId=bk1").expect("tracks");
    let _ = runner.task_outcome(tracks, ok(TRACKS));
    let commands = runner.action(action_id("row-0"));
    let screen = last_screen(&commands).expect("the player");
    assert!(screen_text(&screen).contains("Picture"), "{screen:?}");
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());
}

#[test]
fn a_cover_that_arrives_after_play_starts_reaches_the_player() {
    let (mut runner, commands) = books_shelf(false);
    let (cover, _) = cover_tasks(&commands)[0].clone();
    let (tracks, _) = asked(&runner.action(action_id("tile-0")), "ParentId=bk1").expect("tracks");
    let _ = runner.task_outcome(tracks, ok(TRACKS));
    let commands = runner.action(action_id("row-0"));
    assert!(!screen_text(&last_screen(&commands).expect("the player")).contains("Picture"));
    let commands = runner.task_outcome(cover, cover_bytes());
    let screen = last_screen(&commands).expect("the player again");
    assert!(screen_text(&screen).contains("Picture"), "{screen:?}");
}

/// Answers every cover on its way, and the ones those start, with a picture.
/// Returns the commands last produced and the handles dropped on the way.
fn settle(
    runner: &mut AppRunner<Pharos>,
    mut commands: Vec<Command>,
) -> (Vec<Command>, Vec<PictureHandle>) {
    let mut dropped = Vec::new();
    while !cover_tasks(&commands).is_empty() {
        let mut next = Vec::new();
        for (task, _) in cover_tasks(&commands) {
            let reply = runner.task_outcome(task, cover_bytes());
            dropped.extend(drops(&reply));
            next.extend(reply);
        }
        commands = next;
    }
    (commands, dropped)
}

#[test]
fn the_playing_cover_survives_paging_through_a_long_shelf() {
    let (mut runner, commands) = art_shelf(false, 120);
    let _ = settle(&mut runner, commands);
    let commands = runner.action(action_id("tile-0"));
    let (tracks, _) = asked(&commands, "ParentId=a0").expect("tracks");
    let _ = runner.task_outcome(tracks, ok(TRACKS));
    let commands = runner.action(action_id("row-0"));
    // The report goes through, leaving the player's two timers running.
    let (report, _) = asked(&commands, "/Sessions/Playing").expect("started report");
    let _ = runner.task_outcome(report, ok("{}"));
    let _ = runner.action(ActionId::BACK);
    let commands = runner.action(ActionId::BACK);
    let mut dropped = Vec::new();
    let (_, more) = settle(&mut runner, commands);
    dropped.extend(more);
    for _ in 0..14 {
        let commands = runner.action(action_id("page-next"));
        dropped.extend(drops(&commands));
        let (_, more) = settle(&mut runner, commands.clone());
        dropped.extend(more);
    }
    assert!(!dropped.is_empty(), "other covers were let go");
    // Handles are numbered from one in the order covers arrive; the first
    // to arrive is the album now playing.
    assert!(
        !dropped.contains(&PictureHandle(1)),
        "the cover the player holds was kept"
    );
}

mod about_flow;
mod book_flow;
mod filter_flow;
mod home_flow;

/// `count` albums with covers from `start`, out of `total` on the server.
fn album_block(start: usize, count: usize, total: usize) -> String {
    let items: Vec<String> = (start..start + count)
        .map(|index| {
            format!(
                r#"{{"Id":"a{index}","Name":"Album {index}","AlbumArtist":"Artist A","ImageTags":{{"Primary":"t{index}"}}}}"#
            )
        })
        .collect();
    format!(
        r#"{{"Items":[{}],"TotalRecordCount":{total}}}"#,
        items.join(",")
    )
}

fn draws(commands: &[Command]) -> usize {
    commands
        .iter()
        .filter(|command| matches!(command, Command::SetScreen(_)))
        .count()
}

#[test]
fn a_shelf_page_opens_in_one_draw_across_blocks_and_cover_waves() {
    let mut runner = settled_home(false);
    let mut all = runner.action(action_id("section-music"));
    assert_eq!(draws(&all), 0, "opening the section waits for the page");
    let (first, _) = asked(&all, "StartIndex=0&").expect("the first block");
    let reply = runner.task_outcome(first, ok(&album_block(0, 100, 130)));
    assert!(
        asked(&reply, "StartIndex=100&").is_none(),
        "the next block waits until the page showing is drawn"
    );
    let mut pending = reply.clone();
    all.extend(reply);
    let mut covers = 0;
    while let Some((task, work)) = spawns(&pending).into_iter().find(|(_, work)| {
        url_of(work)
            .is_some_and(|url| url.contains("/Images/Primary") || url.contains("StartIndex=100&"))
    }) {
        pending.retain(
            |command| !matches!(command, Command::Spawn { task: held, .. } if *held == task),
        );
        let outcome = if url_of(&work).is_some_and(|url| url.contains("StartIndex=100&")) {
            ok(&album_block(100, 30, 130))
        } else {
            covers += 1;
            cover_bytes()
        };
        let mut reply = runner.task_outcome(task, outcome);
        let cancelled = answer_cancels(&mut runner, &reply);
        reply.extend(cancelled);
        pending.extend(reply.clone());
        all.extend(reply);
    }
    assert!(
        covers > 2,
        "the covers came in more than one wave: {covers}"
    );
    assert!(
        asked(&all, "StartIndex=100&").is_some(),
        "the next block was asked for once the page was drawn"
    );
    assert_eq!(
        draws(&all),
        1,
        "the page is drawn once with its albums and covers; covers for \
         other pages and the later block draw nothing"
    );
    let showing = screen_text(&last_screen(&all).expect("the page"));
    assert!(!showing.contains("picture: None"), "{showing}");
}

fn logged(commands: &[Command], part: &str) -> bool {
    commands
        .iter()
        .any(|command| matches!(command, Command::Log { message, .. } if message.contains(part)))
}

#[test]
fn each_draw_decision_is_logged_with_what_prompted_it() {
    let mut runner = settled_home(false);
    let opened = runner.action(action_id("section-music"));
    assert!(
        logged(&opened, "pharos: draw held view=shelf cause=input"),
        "{opened:?}"
    );
    let (first, _) = asked(&opened, "StartIndex=0&").expect("the first block");
    let reply = runner.task_outcome(first, ok(&album_block(0, 3, 3)));
    assert!(
        logged(&reply, "pharos: draw held view=shelf cause=shelf"),
        "{reply:?}"
    );
    assert!(logged(&reply, "covers_pending=true"), "{reply:?}");
    let drawn = page_drawn_commands(&mut runner, &reply);
    assert!(
        logged(&drawn, "pharos: draw drawn view=shelf cause=cover"),
        "{drawn:?}"
    );
}

/// Answers covers until the page is drawn, returning every command on the way.
fn page_drawn_commands(runner: &mut AppRunner<Pharos>, commands: &[Command]) -> Vec<Command> {
    let mut all = Vec::new();
    let mut pending = cover_tasks(commands);
    while !pending.is_empty() {
        let (task, _) = pending.remove(0);
        let reply = runner.task_outcome(task, cover_bytes());
        let drawn = last_screen(&reply).is_some();
        pending.extend(cover_tasks(&reply));
        all.extend(reply);
        if drawn {
            break;
        }
    }
    all
}

/// `count` books with covers, out of `total`, as the book list sends them.
fn book_block(count: usize, total: usize) -> String {
    let items: Vec<String> = (0..count)
        .map(|index| {
            format!(
                r#"{{"Id":"b{index}","Name":"Book {index}","Path":"/b/{index}.epub","ImageTags":{{"Primary":"c{index}"}}}}"#
            )
        })
        .collect();
    format!(
        r#"{{"Items":[{}],"TotalRecordCount":{total}}}"#,
        items.join(",")
    )
}

#[test]
fn the_book_list_opens_in_one_draw_with_its_covers() {
    let mut runner = settled_home(false);
    let _ = runner.store_result(StoreResult::Loaded {
        key: books::KEY.to_owned(),
        value: None,
    });
    let mut all = runner.action(action_id("section-books"));
    assert_eq!(draws(&all), 0, "opening the books waits for the page");
    let (list, _) = asked(&all, "IncludeItemTypes=Book").expect("the book list");
    let reply = runner.task_outcome(list, ok(&book_block(40, 40)));
    assert_eq!(draws(&reply), 0, "the list waits for its covers");
    all.extend(page_drawn_commands(&mut runner, &reply));
    all.extend(reply);
    assert_eq!(
        draws(&all),
        1,
        "one draw for the list and its covers (B1458)"
    );
    let showing = screen_text(&last_screen(&all).expect("the list"));
    assert!(!showing.contains("picture: None"), "{showing}");
}

/// A music shelf of `a` albums under A, then `m` under M, then `z` whose
/// names start with an article before a Z.
fn lettered_shelf(a: usize, m: usize, z: usize) -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, _) = home();
    let commands = runner.action(action_id("section-music"));
    let (albums, _) = asked(&commands, "ParentId=lib-music").expect("albums");
    let items: Vec<String> = (0..a + m + z)
        .map(|index| {
            let name = if index < a {
                format!("Apple {index}")
            } else if index < a + m {
                format!("Mango {index}")
            } else {
                format!("The Zest {index}")
            };
            format!(r#"{{"Id":"a{index}","Name":"{name}","AlbumArtist":"Band"}}"#)
        })
        .collect();
    let total = items.len();
    let listing = format!(
        r#"{{"Items":[{}],"TotalRecordCount":{total}}}"#,
        items.join(",")
    );
    let commands = runner.task_outcome(albums, ok(&listing));
    (runner, commands)
}

#[test]
fn the_shelf_jumps_to_the_first_album_under_a_letter() {
    let (mut runner, _) = lettered_shelf(30, 30, 30);
    let commands = runner.action(action_id("letters"));
    let picker = screen_text(&last_screen(&commands).expect("the picker"));
    assert!(picker.contains("Jump to a letter"), "{picker}");
    for offered in ["\"A\"", "\"M\"", "\"Z\""] {
        assert!(picker.contains(offered), "{offered} is offered: {picker}");
    }
    assert!(
        !picker.contains("\"B\""),
        "a letter with no album is not offered"
    );
    let commands = runner.action(action_id("letter-Z"));
    let shelf = screen_text(&last_screen(&commands).expect("the shelf"));
    assert!(!shelf.contains("Jump to a letter"), "the picker closes");
    assert!(
        shelf.contains("The Zest 60"),
        "the first Z album shows: {shelf}"
    );
    assert!(
        !shelf.contains("Apple 29"),
        "the shelf turned to the page Z starts on"
    );
    // Back from the picker leaves the shelf where it was.
    let _ = runner.action(action_id("letters"));
    let commands = runner.action(ActionId::BACK);
    let shelf = screen_text(&last_screen(&commands).expect("the shelf"));
    assert!(!shelf.contains("Jump to a letter"));
    assert!(shelf.contains("The Zest 60"), "{shelf}");
}
