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
    let (mut runner, commands) = started(Some(&account()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let _ = runner.task_outcome(albums, ok(ALBUMS));
    let commands = runner.action(action_id("row-0"));
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
    let commands = runner.action(action_id("row-1"));
    assert!(
        asked(&commands, "ParentId=al2").is_some(),
        "the second album loads"
    );
    let screen = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(screen.contains("Loading the tracks"), "{screen}");
    assert!(screen.contains("Playing"), "{screen}");
}

fn device_requests(commands: &[Command]) -> Vec<DeviceRequest> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::Device(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn saved(commands: &[Command], wanted: &str) -> Option<Vec<u8>> {
    commands.iter().find_map(|command| match command {
        Command::Store(StoreRequest::Save { key, value }) if key == wanted => Some(value.clone()),
        _ => None,
    })
}

fn forgotten(commands: &[Command], wanted: &str) -> bool {
    commands.iter().any(
        |command| matches!(command, Command::Store(StoreRequest::Forget { key }) if key == wanted),
    )
}

/// The runner with an album's tracks listed, and what the runtime then said
/// about their downloads: `first` and `second` as (have, total).
fn album_with_downloads(
    first: Option<(u64, u64)>,
    second: Option<(u64, u64)>,
) -> (AppRunner<Pharos>, Vec<Command>) {
    let (mut runner, commands) = started(Some(&account()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let _ = runner.task_outcome(albums, ok(ALBUMS));
    let commands = runner.action(action_id("row-0"));
    let (tracks, _) = asked(&commands, "ParentId=al1").expect("tracks");
    let commands = runner.task_outcome(tracks, ok(TRACKS));
    let urls = device_requests(&commands)
        .into_iter()
        .find_map(|request| match request {
            DeviceRequest::ListDownloads { urls } => Some(urls),
            _ => None,
        })
        .expect("the runtime is asked what is downloaded");
    assert_eq!(urls.len(), 2);
    assert!(urls[0].contains("/Audio/tr1/universal") && urls[1].contains("/Audio/tr2/universal"));
    let entries = [first, second]
        .iter()
        .zip(&urls)
        .filter_map(|(kept, url)| {
            kept.map(|(have, total)| kobo_sdk::DownloadEntry {
                url: url.clone(),
                have,
                total,
            })
        })
        .collect();
    let commands = runner.device_result(DeviceResult::Downloads { entries });
    (runner, commands)
}

#[test]
fn downloaded_tracks_are_marked_and_the_album_is_saved_for_listing_offline() {
    let (_, commands) = album_with_downloads(Some((900, 900)), Some((300, 1_000)));
    let screen = last_screen(&commands).expect("a screen");
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());
    let text = screen_text(&screen);
    assert!(text.contains("Downloaded"), "{text}");
    assert!(text.contains("30%"), "{text}");
    assert!(text.contains("Remove downloads"), "{text}");
    let value = saved(&commands, OFFLINE_KEY).expect("the album is saved");
    let text = String::from_utf8(value).expect("text");
    assert!(
        text.contains("First Album") && text.contains("Opening"),
        "{text}"
    );
}

#[test]
fn an_album_with_nothing_downloaded_says_nothing_and_is_not_saved() {
    let (_, commands) = album_with_downloads(None, None);
    if let Some(screen) = last_screen(&commands) {
        let text = screen_text(&screen);
        assert!(
            !text.contains("Downloaded") && !text.contains("Remove downloads"),
            "{text}"
        );
    }
    assert!(saved(&commands, OFFLINE_KEY).is_none());
}

#[test]
fn removing_downloads_asks_for_each_kept_track_and_then_asks_again() {
    let (mut runner, _) = album_with_downloads(Some((900, 900)), None);
    let commands = runner.action(action_id("remove-downloads"));
    let requests = device_requests(&commands);
    assert!(matches!(
        &requests[0],
        DeviceRequest::RemoveDownload { url } if url.contains("/Audio/tr1/universal")
    ));
    assert_eq!(requests.len(), 2, "only the kept track is removed");
    assert!(matches!(requests[1], DeviceRequest::ListDownloads { .. }));
    // Nothing is kept once the runtime says so: the album is forgotten.
    let _ = runner.device_result(DeviceResult::Done);
    let commands = runner.device_result(DeviceResult::Downloads {
        entries: Vec::new(),
    });
    assert!(forgotten(&commands, OFFLINE_KEY));
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(!text.contains("Remove downloads"), "{text}");
}

fn offline_library() -> Vec<u8> {
    let mut offline = Offline::default();
    offline.keep(
        &Entry {
            id: "al1".to_owned(),
            name: "First Album".to_owned(),
            by: "Band".to_owned(),
            runtime_ms: None,
        },
        &[Entry {
            id: "tr1".to_owned(),
            name: "Opening".to_owned(),
            by: String::new(),
            runtime_ms: Some(187_000),
        }],
    );
    offline.encode()
}

/// Start-up as the runtime delivers it: the saved albums, the unsent places,
/// then the account.
fn started_with_saved(
    library: Option<Vec<u8>>,
    unsent: Option<Vec<u8>>,
) -> (AppRunner<Pharos>, Vec<Command>) {
    let mut runner = AppRunner::new(Pharos::default());
    let _ = runner.start();
    for (key, value) in [(OFFLINE_KEY, library), (UNSENT_KEY, unsent)] {
        let _ = runner.store_result(StoreResult::Loaded {
            key: key.to_owned(),
            value,
        });
    }
    let commands = runner.store_result(StoreResult::Loaded {
        key: ACCOUNT_KEY.to_owned(),
        value: Some(account().encode()),
    });
    (runner, commands)
}

#[test]
fn with_the_server_out_of_reach_the_downloaded_albums_are_listed_and_play() {
    let (mut runner, commands) = started_with_saved(Some(offline_library()), None);
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let commands = runner.task_outcome(albums, TaskOutcome::Failed(TaskError::Offline));
    let screen = last_screen(&commands).expect("a screen");
    let text = screen_text(&screen);
    assert!(
        text.contains("First Album") && text.contains("Downloaded"),
        "{text}"
    );
    assert!(text.contains("offline"), "{text}");
    assert!(!text.contains("could not be reached"), "{text}");
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());

    // Opening it needs no server: the tracks were saved with it.
    let commands = runner.action(action_id("row-0"));
    assert!(
        spawns(&commands)
            .iter()
            .all(|(_, task)| url_of(task).is_none()),
        "asked the server: {commands:?}"
    );
    let screen = last_screen(&commands).expect("a screen");
    assert!(screen_text(&screen).contains("Opening"));
    assert!(screen.validate(&CLARA_BW_METRICS).is_empty());
    let commands = runner.action(action_id("row-0"));
    // The player is up and playing from the saved listing; it reads the
    // audio and Bluetooth state before it loads the stream.
    assert!(device_requests(&commands).contains(&DeviceRequest::ReadAudio));
}

#[test]
fn with_the_server_out_of_reach_and_nothing_downloaded_it_says_so() {
    let (mut runner, commands) = started_with_saved(None, None);
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let commands = runner.task_outcome(albums, TaskOutcome::Failed(TaskError::Offline));
    let text = screen_text(&last_screen(&commands).expect("a screen"));
    assert!(text.contains("offline"), "{text}");
}

#[test]
fn a_place_the_server_did_not_hear_is_kept_and_sent_when_it_answers() {
    let (mut runner, commands) = playing();
    let (report, _) = asked(&commands, "/Sessions/Playing").expect("started report");
    let commands = runner.task_outcome(report, TaskOutcome::Failed(TaskError::Offline));
    let value = saved(&commands, UNSENT_KEY).expect("the place is kept");
    assert_eq!(String::from_utf8(value).expect("text"), "tr2\t0\n");

    // A fresh start with the place in the store: the first answer from the
    // server sends it, and a sent place is forgotten.
    let (mut runner, commands) = started_with_saved(None, Some(b"tr9\t123456\n".to_vec()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let commands = runner.task_outcome(albums, ok(ALBUMS));
    let (replay, task) = asked(&commands, "/Sessions/Playing/Stopped").expect("sent");
    let Task::Post { body, .. } = task else {
        panic!("a report is a POST");
    };
    assert!(
        body.contains("\"ItemId\":\"tr9\"") && body.contains("1234560000"),
        "{body}"
    );
    let commands = runner.task_outcome(replay, ok(""));
    assert!(forgotten(&commands, UNSENT_KEY));
}

#[test]
fn a_place_that_fails_to_send_stays_kept() {
    let (mut runner, commands) = started_with_saved(None, Some(b"tr9\t5\n".to_vec()));
    let (albums, _) = asked(&commands, "MusicAlbum").expect("albums");
    let commands = runner.task_outcome(albums, ok(ALBUMS));
    let (replay, _) = asked(&commands, "/Sessions/Playing/Stopped").expect("sent");
    let commands = runner.task_outcome(replay, TaskOutcome::Failed(TaskError::TimedOut));
    assert!(!forgotten(&commands, UNSENT_KEY));
}

#[test]
fn signing_out_forgets_the_saved_albums_and_unsent_places() {
    let (mut runner, _) = started_with_saved(Some(offline_library()), Some(b"tr9\t5\n".to_vec()));
    let commands = runner.action(action_id("sign-out"));
    assert!(forgotten(&commands, OFFLINE_KEY) && forgotten(&commands, UNSENT_KEY));
}
