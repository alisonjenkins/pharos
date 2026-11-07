//! Spec 028 FR-017 (B604): the "English (Pharos styled)" track is listed in
//! the native detail only while a current plan exists, and never in the
//! Jellyfin streams.

use std::path::Path;

use pharos_api::pharos_capnp::{detail_result, session};
use pharos_core::{
    LibraryKind, LibraryStore as _, MediaItem, MediaKind, MediaStore as _, SubtitleStyling,
    SubtitleStylingStore as _, SubtitleTrack,
};
use pharos_server::styled_track::styled_index;

use crate::common::Fixture;

pub(crate) const SCRIPT: &str = "[Script Info]\nScriptType: v4.00+\n\n[Events]\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,Hello there\n\
Dialogue: 0,0:00:03.00,0:00:04.50,Default,,0,0,0,,{\\i1}Second line{\\i0}\n";

/// Whole-second mtime and size, as the styling job records a file.
pub(crate) fn signature(path: &Path) -> (i64, u64) {
    let m = std::fs::metadata(path).unwrap();
    let secs = m
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (i64::try_from(secs).unwrap(), m.len())
}

/// A styling row for `item` whose plan directs [`SCRIPT`], made from
/// `source_index` (a sidecar when `sidecar` is given).
pub(crate) fn styling_row(
    item: &MediaItem,
    source_index: u32,
    sidecar: Option<&Path>,
) -> SubtitleStyling {
    let source = pharos_styling::source::read(SCRIPT).unwrap();
    let plan = pharos_styling::direct(&source.lines, &Default::default(), &[]).plan;
    let (source_mtime, source_size) = signature(&item.path);
    let sidecar_sig = sidecar.map(signature);
    SubtitleStyling {
        item_id: item.id,
        source_index,
        source_mtime,
        source_size,
        sidecar_mtime: sidecar_sig.map(|s| s.0),
        sidecar_size: sidecar_sig.map(|s| s.1),
        director_version: pharos_styling::DIRECTOR_VERSION,
        outcome: "styled".into(),
        plan_json: Some(serde_json::to_string(&plan).unwrap()),
        styled_at: 1_700_000_100,
    }
}

struct Listed {
    index: u32,
    title: String,
    language: String,
    codec: String,
    url: String,
    styled_url: String,
    is_default: bool,
}

async fn subtitles(session: &session::Client, id: u64) -> Vec<Listed> {
    let library = session.library_request().send().pipeline.get_library();
    let mut req = library.detail_request();
    req.get().init_ctx();
    req.get().init_id().set_real(id);
    let reply = req.send().promise.await.expect("detail");
    let detail_result::Detail(d) = reply.get().unwrap().get_result().unwrap().which().unwrap()
    else {
        panic!("detail refused");
    };
    let text = |t: capnp::Result<capnp::text::Reader<'_>>| t.unwrap().to_str().unwrap().to_owned();
    d.unwrap()
        .get_subtitle_tracks()
        .unwrap()
        .iter()
        .map(|t| Listed {
            index: t.get_index(),
            title: text(t.get_title()),
            language: text(t.get_language()),
            codec: text(t.get_codec()),
            url: text(t.get_url()),
            styled_url: text(t.get_styled_url()),
            is_default: t.get_is_default(),
        })
        .collect()
}

#[actix_web::test]
async fn the_styled_track_is_listed_only_while_a_current_plan_exists() {
    let (server, fx) = crate::revocation::start().await;
    let (client, _closed) = crate::revocation::connect(&server).await;
    let session = crate::revocation::signed_in(&client, &fx.user_token).await;
    let (dir, item) = seed(&fx, 401).await;
    let sidecar = dir.path().join("Show.eng.ass");
    // Embedded ass at 2, then the sidecar at 3: the styled slot is 4.
    let slot = styled_index(&item.probe, 1);
    assert_eq!(slot, 4);

    let plain = subtitles(&session, 401).await;
    assert!(plain.iter().all(|t| t.styled_url.is_empty()), "no row");
    assert_eq!(plain.len(), 2);

    let row = styling_row(&item, 3, Some(&sidecar));
    fx.state
        .stores
        .put_subtitle_styling(row.clone())
        .await
        .unwrap();
    let listed = subtitles(&session, 401).await;
    assert_eq!(listed.len(), 3);
    let styled = listed.last().unwrap();
    assert_eq!(styled.index, slot);
    assert_eq!(styled.title, "English (Pharos styled)");
    assert_eq!(styled.language, "eng");
    assert_eq!(styled.codec, "ass");
    assert!(!styled.is_default);
    let wire = pharos_jellyfin_api::dto::wire_item_id(401);
    assert_eq!(styled.url, format!("/videos/{wire}/subtitles/3/stream.vtt"));
    assert_eq!(
        styled.styled_url,
        format!("/videos/{wire}/subtitles/4/stream.ass")
    );
    assert!(
        listed[..2].iter().all(|t| t.styled_url.is_empty()),
        "only the generated track carries a styled URL"
    );

    // Jellyfin's streams never list the slot.
    let item = fx.state.stores.get(401).await.unwrap();
    let streams = pharos_server::services::watch::streams(&item).await;
    assert!(
        streams.iter().all(|s| s.index != slot),
        "MediaStreams must not list the styled track"
    );

    // An old director version is not served.
    let mut old = row.clone();
    old.director_version = pharos_styling::DIRECTOR_VERSION + 1;
    fx.state.stores.put_subtitle_styling(old).await.unwrap();
    assert_eq!(subtitles(&session, 401).await.len(), 2, "director version");
    // A version-only row (no plan) is not served.
    let mut empty = row.clone();
    empty.plan_json = None;
    fx.state.stores.put_subtitle_styling(empty).await.unwrap();
    assert_eq!(subtitles(&session, 401).await.len(), 2, "no plan");

    // A sidecar edited after the plan was made.
    fx.state
        .stores
        .put_subtitle_styling(row.clone())
        .await
        .unwrap();
    assert_eq!(subtitles(&session, 401).await.len(), 3);
    std::fs::write(&sidecar, format!("{SCRIPT}; edited\n")).unwrap();
    assert_eq!(subtitles(&session, 401).await.len(), 2, "sidecar changed");

    // The video file replaced after the plan was made.
    std::fs::write(&sidecar, SCRIPT).unwrap();
    fx.state
        .stores
        .put_subtitle_styling(styling_row(&item, 3, Some(&sidecar)))
        .await
        .unwrap();
    assert_eq!(subtitles(&session, 401).await.len(), 3);
    std::fs::write(&item.path, b"a different, longer video").unwrap();
    assert_eq!(subtitles(&session, 401).await.len(), 2, "video changed");
}

/// An item in its own library: `Show.mkv` with an embedded ASS at 2 and the
/// sidecar `Show.eng.ass` (index 3).
pub(crate) async fn seed(fx: &Fixture, id: u64) -> (tempfile::TempDir, MediaItem) {
    let dir = tempfile::TempDir::new().unwrap();
    fx.state
        .stores
        .upsert_library(
            "Scratch",
            dir.path().to_str().unwrap(),
            LibraryKind::Movies,
            &"c".repeat(32),
        )
        .await
        .unwrap();
    std::fs::write(dir.path().join("Show.mkv"), b"x").unwrap();
    std::fs::write(dir.path().join("Show.eng.ass"), SCRIPT).unwrap();
    let mut item = MediaItem {
        id,
        path: dir.path().join("Show.mkv"),
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
    fx.state.stores.put(item.clone()).await.unwrap();
    (dir, item)
}
