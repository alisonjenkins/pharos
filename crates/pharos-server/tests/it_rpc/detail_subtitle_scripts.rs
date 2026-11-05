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
