use super::*;
use kobo_sdk::{AppRunner, Command, StoreRequest, CLARA_BW_METRICS};

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
fn started(saved: Option<&Account>) -> (AppRunner<Pharos>, Vec<Command>) {
    let mut runner = AppRunner::new(Pharos::default());
    let _ = runner.start();
    let commands = runner.store_result(StoreResult::Loaded {
        key: ACCOUNT_KEY.to_owned(),
        value: saved.map(Account::encode),
    });
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
fn quick_connect_signs_in_keeps_the_account_and_lists_albums() {
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
    let (_, task) =
        asked(&commands, "/Users/u1/Items?IncludeItemTypes=MusicAlbum").expect("albums");
    let Task::Fetch { headers, .. } = task else {
        panic!("albums is a GET");
    };
    assert!(headers
        .iter()
        .any(|header| header.value.contains("Token=\"t0k\"")));
}

#[test]
fn a_saved_account_goes_straight_to_the_albums() {
    let (_, commands) = started(Some(&account()));
    assert!(asked(&commands, "/QuickConnect").is_none());
    assert!(asked(&commands, "IncludeItemTypes=MusicAlbum").is_some());
}

#[test]
fn a_rejected_token_is_forgotten_and_sign_in_starts_again() {
    let (mut runner, commands) = started(Some(&account()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let commands = runner.task_outcome(albums, TaskOutcome::Failed(TaskError::Unauthorized));
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
    let (mut runner, commands) = started(Some(&account()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let _ = runner.task_outcome(albums, ok(ALBUMS));
    let commands = runner.action(action_id("row-0"));
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
    let commands = runner.action(action_id("sign-out"));
    assert!(asked(&commands, "/Sessions/Playing/Stopped").is_some());
    assert!(commands.iter().any(|command| matches!(
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
    let (mut runner, commands) = started(Some(&account()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let commands = runner.task_outcome(
        albums,
        ok(&format!(r#"{{"Items":[{many}],"TotalRecordCount":20}}"#)),
    );
    screens.extend(last_screen(&commands));
    let commands = runner.action(action_id("row-0"));
    screens.extend(last_screen(&commands));
    let (tracks, _) = asked(&commands, "ParentId=a0").expect("tracks");
    let commands = runner.task_outcome(tracks, ok(TRACKS));
    screens.extend(last_screen(&commands));

    assert_eq!(screens.len(), 5);
    for (index, screen) in screens.iter().enumerate() {
        let problems = screen.validate(&CLARA_BW_METRICS);
        assert!(problems.is_empty(), "screen {index}: {problems:?}");
    }
}

#[test]
fn tracks_for_an_album_already_left_are_ignored() {
    let (mut runner, commands) = started(Some(&account()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let _ = runner.task_outcome(albums, ok(ALBUMS));
    let commands = runner.action(action_id("row-0"));
    let (first, _) = asked(&commands, "ParentId=al1").expect("first album's tracks");
    let _ = runner.action(ActionId::BACK);
    let commands = runner.action(action_id("row-1"));
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
    let (mut runner, commands) = started(Some(&account()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let _ = runner.task_outcome(albums, ok(ALBUMS));
    let commands = runner.action(action_id("row-0"));
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
