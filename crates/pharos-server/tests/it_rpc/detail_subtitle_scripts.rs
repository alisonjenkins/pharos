//! 026 FR-012 / V489 (B572): a `.ass` or `.ssa` sidecar is listed with its
//! real codec, so the player draws it styled from `stream.ass`; other
//! sidecars stay WebVTT.

use pharos_api::pharos_capnp::detail_result;
use pharos_core::{
    LibraryKind, LibraryStore as _, MediaItem, MediaKind, MediaStore as _, SubtitleTrack,
};

#[actix_web::test]
async fn sidecar_scripts_are_listed_as_ass_and_ssa_and_the_rest_as_webvtt() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;

    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().to_str().unwrap();
    fx.state
        .stores
        .upsert_library("Scratch", root, LibraryKind::Movies, &"c".repeat(32))
        .await
        .unwrap();
    let media = dir.path().join("Show.mkv");
    for name in ["Show.mkv", "Show.eng.ass", "Show.fra.ssa", "Show.deu.srt"] {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    let mut item = MediaItem {
        id: 301,
        path: media,
        title: "Show".into(),
        kind: MediaKind::Movie,
        ..MediaItem::default()
    };
    item.probe.duration_ms = Some(60_000);
    item.probe.video_codec = Some("h264".into());
    item.probe.subtitle_tracks = vec![SubtitleTrack {
        stream_index: 2,
        codec: Some("ass".into()),
        language: Some("jpn".into()),
        ..SubtitleTrack::default()
    }];
    fx.state.stores.put(item).await.unwrap();

    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.detail_request();
    req.get().init_ctx();
    req.get().init_id().set_real(301);
    let reply = req.send().promise.await.expect("detail");
    let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("detail refused");
    };
    let codecs: Vec<(u32, String)> = d
        .unwrap()
        .get_subtitle_tracks()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t.get_index(),
                t.get_codec().unwrap().to_str().unwrap().to_owned(),
            )
        })
        .collect();
    // The embedded track is index 2; sidecars follow in path order:
    // Show.deu.srt, Show.eng.ass, Show.fra.ssa.
    assert_eq!(
        codecs,
        [
            (2, "ass".to_owned()),
            (3, "webvtt".to_owned()),
            (4, "ass".to_owned()),
            (5, "ssa".to_owned()),
        ],
        "V489: a sidecar script is not labelled webvtt"
    );
}

/// 026 FR-010 (B572, V488): the detail lists the file's fonts, with the URL
/// template the client fills the subtitle index into, and leaves out
/// attachments that are not fonts.
#[actix_web::test]
async fn detail_lists_the_embedded_fonts_and_not_other_attachments() {
    use pharos_core::MediaAttachment;

    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let mut item = MediaItem {
        id: 302,
        path: "/libA/Fonted.mkv".into(),
        title: "Fonted".into(),
        kind: MediaKind::Movie,
        ..MediaItem::default()
    };
    item.probe.attachments = vec![
        MediaAttachment {
            stream_index: 4,
            filename: Some("Arial.ttf".into()),
            mime_type: Some("application/x-truetype-font".into()),
            codec: Some("ttf".into()),
        },
        MediaAttachment {
            stream_index: 5,
            filename: Some("cover.jpg".into()),
            mime_type: Some("image/jpeg".into()),
            codec: None,
        },
        MediaAttachment {
            stream_index: 6,
            filename: Some("Mincho.OTF".into()),
            mime_type: Some("application/octet-stream".into()),
            codec: None,
        },
    ];
    fx.state.stores.put(item).await.unwrap();

    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.detail_request();
    req.get().init_ctx();
    req.get().init_id().set_real(302);
    let reply = req.send().promise.await.expect("detail");
    let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("detail refused");
    };
    let text = |t: capnp::Result<capnp::text::Reader<'_>>| t.unwrap().to_str().unwrap().to_owned();
    let fonts: Vec<(u32, String, String, String)> = d
        .unwrap()
        .get_fonts()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f.get_index(),
                text(f.get_filename()),
                text(f.get_mime()),
                text(f.get_url()),
            )
        })
        .collect();
    let id = pharos_jellyfin_api::dto::wire_item_id(302);
    assert_eq!(
        fonts,
        [
            (
                4,
                "Arial.ttf".to_owned(),
                "application/x-truetype-font".to_owned(),
                format!("/videos/{id}/subtitles/{{subtitle}}/fonts/4"),
            ),
            (
                6,
                "Mincho.OTF".to_owned(),
                "font/otf".to_owned(),
                format!("/videos/{id}/subtitles/{{subtitle}}/fonts/6"),
            ),
        ],
        "V488: the detail does not list the file's fonts"
    );
}

/// B591 (V509): a song's detail names its track artists and album, for the
/// now-playing screen; a film's names neither.
#[actix_web::test]
async fn a_songs_detail_names_its_artists_and_album() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let mut song = MediaItem {
        id: 303,
        path: "/libA/Teardrop.flac".into(),
        title: "Teardrop".into(),
        kind: MediaKind::Audio,
        ..MediaItem::default()
    };
    song.probe.artist = Some("Massive Attack".into());
    song.probe.album = Some("Mezzanine".into());
    fx.state.stores.put(song).await.unwrap();
    fx.state
        .stores
        .put(MediaItem {
            id: 304,
            path: "/libA/Plain.mkv".into(),
            title: "Plain".into(),
            kind: MediaKind::Movie,
            ..MediaItem::default()
        })
        .await
        .unwrap();

    let library = session.library_request().send().pipeline.get_library();
    let text = |t: capnp::Result<capnp::text::Reader<'_>>| t.unwrap().to_str().unwrap().to_owned();
    for (id, artists, album) in [
        (303, vec!["Massive Attack"], "Mezzanine"),
        (304, vec![], ""),
    ] {
        let mut req = library.detail_request();
        req.get().init_ctx();
        req.get().init_id().set_real(id);
        let reply = req.send().promise.await.expect("detail");
        let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
        else {
            panic!("detail refused");
        };
        let d = d.unwrap();
        let got: Vec<String> = d.get_artists().unwrap().iter().map(text).collect();
        assert_eq!(got, artists, "V509: item {id} artists");
        assert_eq!(text(d.get_album()), album, "V509: item {id} album");
    }
}
