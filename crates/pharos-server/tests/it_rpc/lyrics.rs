//! `Library.lyrics` and `GET /Audio/{id}/Lyrics`: one lyrics service behind
//! both wires (B360/V300).

use pharos_api::pharos_capnp::{lyrics_result, session};
use pharos_client::api::{ItemRef, LyricLine, LyricWord, Lyrics, NativeApi, PharosApi as _};
use pharos_client::CallInfo;
use pharos_core::{MediaItem, MediaKind, MediaStore as _, UserPolicy};
use pharos_server::services::{library::DetailError, lyrics};

use crate::common::seed_rich;

/// `(synced, source, [(start_ms, text)])` as `Library.lyrics` answers for `id`.
async fn wire_lyrics(session: &session::Client, id: u64) -> (bool, String, Vec<(u64, String)>) {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.lyrics_request();
    req.get().init_ctx();
    req.get().init_id().set_real(id);
    let reply = req.send().promise.await.expect("lyrics");
    let result = reply.get().unwrap().get_result().unwrap();
    let lyrics_result::Lyrics(lyrics) = result.which().unwrap() else {
        panic!("lyrics answered an error");
    };
    let lyrics = lyrics.unwrap();
    let lines = lyrics
        .get_lines()
        .unwrap()
        .iter()
        .map(|l| (l.get_start_ms(), l.get_text().unwrap().to_string().unwrap()))
        .collect();
    (
        lyrics.get_synced(),
        lyrics.get_source().unwrap().to_string().unwrap(),
        lines,
    )
}

async fn put_song(fx: &crate::common::Fixture, id: u64, path: std::path::PathBuf) {
    fx.state
        .stores
        .put(MediaItem {
            id,
            path,
            title: "Song".into(),
            kind: MediaKind::Audio,
            ..MediaItem::default()
        })
        .await
        .unwrap();
}

/// A song's `.lrc` sidecar comes back synced, a `.txt` one plain, and a song
/// or a film with neither answers no lines rather than an error.
/// Disarm check: with `for_item` returning `Ok(None)` the first assertion fails.
#[actix_web::test]
async fn native_lyrics_serve_sidecars_and_answer_nothing_without_them() {
    let (server, fx) = crate::revocation::start().await;
    let dir = tempfile::tempdir().unwrap();
    put_song(&fx, 400, dir.path().join("synced.mp3")).await;
    std::fs::write(
        dir.path().join("synced.lrc"),
        "[ar:Band]\n[00:01.50]First\n[00:04.00]Second\n",
    )
    .unwrap();
    put_song(&fx, 401, dir.path().join("plain.mp3")).await;
    std::fs::write(dir.path().join("plain.txt"), "Verse one\n\nVerse two\n").unwrap();
    put_song(&fx, 402, dir.path().join("bare.mp3")).await;
    put_song(&fx, 403, dir.path().join("enhanced.mp3")).await;
    std::fs::write(
        dir.path().join("enhanced.lrc"),
        "[00:01.00]<00:01.00>Hello <00:01.50>world<00:02.20>\n",
    )
    .unwrap();

    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.admin_token).await;

    assert_eq!(
        wire_lyrics(&session, 400).await,
        (
            true,
            "sidecar_lrc".to_owned(),
            vec![(1_500, "First".to_owned()), (4_000, "Second".to_owned())]
        )
    );
    let api = NativeApi::new(&session);
    assert_eq!(
        api.lyrics(&CallInfo::default(), ItemRef::Real(403), false)
            .await
            .unwrap(),
        Lyrics {
            synced: true,
            lines: vec![LyricLine {
                start_ms: 1_000,
                text: "Hello world".to_owned(),
                words: vec![
                    LyricWord {
                        at: 0,
                        start_ms: 1_000,
                    },
                    LyricWord {
                        at: 6,
                        start_ms: 1_500,
                    },
                    LyricWord {
                        at: 11,
                        start_ms: 2_200,
                    },
                ],
            }],
            words_pending: false,
        }
    );
    let (synced, source, lines) = wire_lyrics(&session, 401).await;
    assert!(!synced);
    assert_eq!(source, "sidecar_text");
    assert_eq!(
        lines.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(),
        ["Verse one", "", "Verse two"]
    );
    assert_eq!(
        wire_lyrics(&session, 402).await,
        (false, String::new(), vec![])
    );
    assert_eq!(
        wire_lyrics(&session, fx.rich_item_id).await,
        (false, String::new(), vec![])
    );
}

/// The Jellyfin route reads the same service: ticks on synced lines, a null
/// `Start` on plain ones, and `IsSynced` in the metadata.
#[actix_web::test]
async fn jellyfin_lyrics_route_shares_the_service() {
    let fx = seed_rich().await;
    let dir = tempfile::tempdir().unwrap();
    put_song(&fx, 410, dir.path().join("song.flac")).await;
    std::fs::write(dir.path().join("song.lrc"), "[00:02.00]Hello\n").unwrap();
    put_song(&fx, 411, dir.path().join("words.flac")).await;
    std::fs::write(dir.path().join("words.txt"), "Just words\n").unwrap();
    let app = actix_web::test::init_service(crate::common::build_app(fx.state.clone())).await;
    let get = |id: u64| {
        actix_web::test::TestRequest::get()
            .uri(&format!("/Audio/{id}/Lyrics"))
            .insert_header(("X-Emby-Token", fx.admin_token.as_str()))
            .to_request()
    };
    let synced: serde_json::Value = actix_web::test::call_and_read_body_json(&app, get(410)).await;
    assert_eq!(
        synced,
        serde_json::json!({
            "Metadata": { "IsSynced": true },
            "Lyrics": [{ "Text": "Hello", "Start": 20_000_000u64 }]
        })
    );
    let plain: serde_json::Value = actix_web::test::call_and_read_body_json(&app, get(411)).await;
    assert_eq!(
        plain,
        serde_json::json!({
            "Metadata": { "IsSynced": false },
            "Lyrics": [{ "Text": "Just words", "Start": null }]
        })
    );
}

/// A user whose policy hides the item gets no lyrics for it either.
#[actix_web::test]
async fn lyrics_follow_the_users_folder_policy() {
    let fx = seed_rich().await;
    let mut user = pharos_server::services::auth::user_for_token(&fx.state.stores, &fx.user_token)
        .await
        .unwrap();
    user.policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![fx.lib_a_wire.clone()],
        ..user.policy
    };
    let hidden = lyrics::for_item(&fx.state, &user, fx.other_item_id, false).await;
    assert!(matches!(hidden, Err(DetailError::Hidden(_))), "{hidden:?}");
}
